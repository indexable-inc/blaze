//! The CLI's checkouts as heads (docs/v2/plan/lanes/cargo/notes.md section 6): a request that names
//! `ViewRef::Head(<absolute path>)` builds the checkout as saved.
//!
//! - The first request on a checkout imports it: a [`Watcher`] of its directory scans it into
//!   the head named by its path and keeps the head current. What a build writes and what is no
//!   source is not taken in: the root's `target/` and every `.git`.
//! - A request on a checkout waits until the watcher has seen every change made before it, as
//!   watchman's cookies do: the daemon writes a cookie file under the checkout's `target/`
//!   (outside the view) and holds the request until the watcher reports the cookie and has
//!   committed everything it was told before it. Only then does the session see it.
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
//! changes costs one pass over its files: each is matched against the checkout's cookie prefix
//! and filtered in place, nothing allocated per file.

#![forbid(unsafe_code)]

use blaze_api::{ApiError, Arrived, Reply, Request, RequestRef, Response, ViewRef};
use blaze_core::{Completion, Conn, Done, Effect, FileName, Tag, decimal::decimal};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_vfs::{Outside, Vfs, Watched, Watcher};

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
    /// Each checkout it names, by slot, and the cookie it waits for there.
    waits: Vec<(u32, u64)>,
    /// It runs user code: it waits for the round of the watches outside views.
    outside: bool,
}

struct Checkout {
    head: String,
    watcher: Watcher,
    /// Its directory under the driver's root, then `/`: what its files' paths start with.
    below: String,
    /// `<dir>/target`, where its cookies are.
    cookie_dir: String,
    /// `<dir>/target/.blaze-cookie-<pid>-`: a cookie's path is this and its number.
    cookie_prefix: String,
    /// Cookies written or being written and not seen yet, by number. Walked in number order
    /// only where the walk makes effects ([`Heads::sweep`]).
    cookies: HashMap<u64, Cookie>,
}

struct Cookie {
    file: FileName,
    written: bool,
    seen: bool,
}

pub struct Heads {
    /// The cookies' writes and removes: `tags..tags + 2^32`.
    tags: u64,
    /// The first tag of slot 0's watcher.
    watchers: u64,
    /// The last tag of every watcher's range.
    watcher_end: u64,
    next_cookie: u64,
    /// By slot; a slot whose watch ended stays empty, its tags never used again.
    checkouts: Vec<Option<Box<Checkout>>>,
    /// The slot of each checkout watched, by its head.
    slots: HashMap<String, u32>,
    /// By cookie tag: the checkout's slot and the cookie.
    writing: HashMap<Tag, (u32, u64)>,
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
        | Request::Pack { view, .. }
        | Request::Doc { view, .. }
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
/// of their inputs outside views read the watches' round.
pub fn runs_user_code(request: &Request) -> bool {
    matches!(
        request,
        Request::Check { .. }
            | Request::Build { .. }
            | Request::Test { .. }
            | Request::Run { .. }
            | Request::BestOf { .. }
            | Request::Verify { .. }
            | Request::Pack { .. }
            | Request::Doc { .. }
            | Request::WillTest { .. }
            | Request::WillSave { .. }
    )
}

/// Whether the file at `rel` of a checkout is out of its view: the build's `target/` at the
/// root and every `.git`.
pub fn excluded(rel: &str) -> bool {
    rel.strip_prefix("target").is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        || (rel.contains(".git") && rel.split('/').any(|part| part == ".git"))
}

/// Whether `dir` is `at` or under it.
fn within(dir: &str, at: &str) -> bool {
    dir.strip_prefix(at).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Checkout {
    /// Whether `file` is in the view: outside the checkout, or not [`excluded`].
    fn keeps(&self, file: &FileName) -> bool {
        file.as_str().strip_prefix(self.below.as_str()).is_none_or(|rel| !excluded(rel))
    }
}

impl Heads {
    /// Cookies take the tags `tags..tags + 2^32`; watchers take ranges of `2^32` from
    /// `watchers` on, up to `watchers_end`; `outside` its own ([`Outside::owns`]).
    pub fn new(tags: u64, watchers: u64, watchers_end: u64, outside: Outside<'static>) -> Heads {
        Heads {
            tags,
            watchers,
            watcher_end: watchers_end,
            next_cookie: 0,
            checkouts: Vec::new(),
            slots: HashMap::default(),
            writing: HashMap::default(),
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

    /// How many requests wait for a watcher.
    pub fn waiting(&self) -> usize {
        self.held.len()
    }

    /// Takes what arrived: passed on, or held until the watchers of the checkouts it names
    /// have caught up with it.
    pub fn arrive(&mut self, vfs: &Vfs, arrived: Arrived, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        let (from, request) = match arrived {
            Arrived::Gone { conn } => {
                self.held.retain(|held| held.from.conn != conn);
                out.push(Out::Pass(Arrived::Gone { conn }));
                return;
            }
            Arrived::Request { from, request } => (from, request),
        };
        if let Request::Cancel { id } = request
            && let Some(at) = self.held.iter().position(|it| it.from == RequestRef { conn: from.conn, id })
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
        for head in heads {
            match self.cookie(vfs, head, effects) {
                Ok(wait) => waits.push(wait),
                Err(error) => {
                    answer(out, from, Some(error), false);
                    return;
                }
            }
        }
        if outside {
            self.outside.flush(effects);
        }
        self.held.push(Held { from, request, waits, outside });
        self.release(out);
    }

    /// A cookie written into `head`'s checkout, its watcher started first if it has none: the
    /// checkout's slot and the cookie's number.
    fn cookie(&mut self, vfs: &Vfs, head: &str, effects: &mut Vec<Effect>) -> Result<(u32, u64), ApiError> {
        let slot = match self.slots.get(head) {
            Some(&slot) => slot,
            None => self.watch(vfs, head, effects)?,
        };
        let checkout = self.checkouts[slot as usize].as_deref_mut().expect("a slot named is watched");
        let number = self.next_cookie;
        self.next_cookie += 1;
        let mut path = String::with_capacity(checkout.cookie_prefix.len() + 20);
        path.push_str(&checkout.cookie_prefix);
        path.push_str(decimal(number, &mut [0; 20]));
        let file = FileName::new(&path).expect("a cookie's name is a name");
        let tag = Tag(self.tags + (number % (1 << 31)) * 2);
        self.writing.insert(tag, (slot, number));
        path.clear();
        path.push_str(decimal(number, &mut [0; 20]));
        path.push('\n');
        effects.push(Effect::Write { file: file.clone(), bytes: path.into_bytes(), tag });
        checkout.cookies.insert(number, Cookie { file, written: false, seen: false });
        Ok((slot, number))
    }

    /// Starts watching `head`'s checkout in a new slot.
    fn watch(&mut self, vfs: &Vfs, head: &str, effects: &mut Vec<Effect>) -> Result<u32, ApiError> {
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
        watcher.start(vfs, effects);
        let cookie_dir = format!("{dir}/target");
        let cookie_prefix = format!("{cookie_dir}/.blaze-cookie-{}-", std::process::id());
        let checkout = Checkout {
            head: head.to_owned(),
            watcher,
            below: format!("{dir}/"),
            cookie_dir,
            cookie_prefix,
            cookies: HashMap::default(),
        };
        self.checkouts.push(Some(Box::new(checkout)));
        self.slots.insert(head.to_owned(), slot);
        Ok(slot)
    }

    /// Takes a completion of its own; what was held and is now caught up goes on.
    pub fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        if completion.tag.0.wrapping_sub(self.tags) < 1 << 32 {
            self.cookie_done(completion, out);
        } else if self.outside.owns(completion.tag) {
            self.outside.complete(completion, effects);
        } else {
            self.watched(vfs, completion, effects, out);
        }
        self.release(out);
    }

    fn cookie_done(&mut self, completion: Completion, out: &mut Vec<Out>) {
        // Odd tags are the removes of cookies seen: nothing waits for them.
        let Some((slot, number)) = self.writing.remove(&completion.tag) else { return };
        let Some(checkout) = self.checkouts.get_mut(slot as usize).and_then(Option::as_deref_mut) else { return };
        match completion.done {
            Done::Written => {
                if let Some(cookie) = checkout.cookies.get_mut(&number) {
                    cookie.written = true;
                }
            }
            done => {
                let file = checkout.cookies.remove(&number).map(|it| it.file);
                let why = match done {
                    Done::Failed(error) => error.to_string(),
                    other => format!("the driver answered {other:?}"),
                };
                let what = format!(
                    "writing the cookie {} that shows the watcher of {} has caught up: {why}",
                    file.map_or_else(String::new, |it| format!("/{it}")),
                    checkout.head,
                );
                let head = checkout.head.clone();
                self.fail_waiting(|it| *it == (slot, number), &what, &head, out);
            }
        }
    }

    fn watched(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        let Some(slot) = self.slot_of(completion.tag) else { return };
        let Some(checkout) = self.checkouts.get_mut(slot as usize).and_then(Option::as_deref_mut) else { return };
        let done = match completion.done {
            Done::Journaled(mut batch) => {
                // A directory reported for what changed in it, or a subtree whose changes the
                // journal lost, tells every cookie in it.
                let mut told = false;
                for file in &batch.files {
                    let file = file.as_str();
                    match file.strip_prefix(checkout.cookie_prefix.as_str()).and_then(|it| it.parse::<u64>().ok()) {
                        Some(number) => {
                            if let Some(cookie) = checkout.cookies.get_mut(&number) {
                                cookie.seen = true;
                            }
                        }
                        None => told |= within(&checkout.cookie_dir, file),
                    }
                }
                let lost = batch.lost.iter().any(|(under, _)| within(&checkout.cookie_dir, under.as_str()));
                if told || lost {
                    for cookie in checkout.cookies.values_mut() {
                        cookie.seen |= told || cookie.written;
                    }
                }
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
        let watched = checkout.watcher.complete(vfs, Completion { tag: completion.tag, done }, effects);
        let head = &checkout.head;
        let mut ended = None;
        for it in watched {
            match it {
                Watched::Committed(commit) => out.push(Out::Moved { head: head.clone(), root: commit.root }),
                Watched::Lost { .. } | Watched::Scanned => {}
                Watched::Conflict { path } => {
                    eprintln!("blaze serve: {head}/{path} changed on disk and by a commit since; the head keeps the commit's");
                }
                Watched::Unreadable { path, error } if path.is_empty() => {
                    ended = Some(format!("the checkout {head} can't be read: {error}"));
                }
                Watched::Unreadable { path, error } => {
                    ended.get_or_insert_with(|| format!("{head}/{path} changed and can't be read: {error}"));
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
            self.writing.retain(|_, it| it.0 != slot);
            self.fail_waiting(|it| it.0 == slot, &what, &gone.head, out);
        }
    }

    /// Fails every held request with a wait `matches` says, naming `what`.
    fn fail_waiting(&mut self, matches: impl Fn(&(u32, u64)) -> bool, what: &str, head: &str, out: &mut Vec<Out>) {
        for held in self.held.extract_if(.., |it| it.waits.iter().any(&matches)) {
            answer(out, held.from, Some(ApiError::new(what, Some(head.to_owned()))), false);
        }
    }

    /// Hands on every held request whose every checkout has seen its cookie and committed what
    /// it was told before, and, if it runs user code, whose round outside views is in, oldest
    /// first. A request held for an older round waits for the newer one too.
    fn release(&mut self, out: &mut Vec<Out>) {
        let (checkouts, flushed) = (&self.checkouts, self.outside.flushed());
        let caught_up = |(slot, number): &(u32, u64)| {
            checkouts.get(*slot as usize).and_then(Option::as_deref).is_some_and(|it| {
                it.watcher.idle() && it.cookies.get(number).is_some_and(|cookie| cookie.seen)
            })
        };
        for held in self.held.extract_if(.., |it| (flushed || !it.outside) && it.waits.iter().all(&caught_up)) {
            out.push(Out::Pass(Arrived::Request { from: held.from, request: held.request }));
        }
    }

    /// Removes the cookies seen that no held request waits for.
    pub fn sweep(&mut self, effects: &mut Vec<Effect>) {
        let (held, tags) = (&self.held, self.tags);
        let mut swept = Vec::new();
        for (slot, checkout) in (0u32..).zip(&mut self.checkouts) {
            let Some(checkout) = checkout.as_deref_mut() else { continue };
            swept.extend(
                checkout
                    .cookies
                    .iter()
                    .filter(|(number, cookie)| cookie.seen && !held.iter().any(|it| it.waits.contains(&(slot, **number))))
                    .map(|(number, _)| *number),
            );
            blaze_ids::radix::sort(&mut swept);
            for number in swept.drain(..) {
                let cookie = checkout.cookies.remove(&number).expect("a cookie just found");
                let tag = Tag(tags + (number % (1 << 31)) * 2 + 1);
                effects.push(Effect::Remove { file: cookie.file, tag });
            }
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
    fn a_cookie_dir_is_within_what_contains_it() {
        assert!(within("w/target", "w/target"));
        assert!(within("w/target", "w"));
        assert!(!within("w/target", "w/tar"));
        assert!(!within("w/target", "w/target/x"));
    }

    #[test]
    fn what_waits_is_what_reads_a_checkout() {
        let head = |it: &str| ViewRef::Head(it.to_owned());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
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
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let from = |id| RequestRef { conn: Conn(1), id };
        let build = |id| Arrived::Request {
            from: from(id),
            request: Request::Build { view: ViewRef::Head("ci/main".to_owned()), targets: targets.clone() },
        };
        let passed = |out: &mut Vec<Out>| -> Vec<u64> {
            out.drain(..)
                .map(|it| match it {
                    Out::Pass(Arrived::Request { from, .. }) => from.id,
                    it => panic!("{it:?}"),
                })
                .collect()
        };

        heads.arrive(&vfs, build(1), &mut effects, &mut out);
        assert_eq!(passed(&mut out), [1], "nothing is watched: the round is in at once");

        dirs.want(std::path::Path::new("/usr/include"));
        heads.arrive(&vfs, build(2), &mut effects, &mut out);
        assert_eq!(passed(&mut out), [2], "a watch not yet confirmed has nothing to flush");
        let stat = effects
            .iter()
            .find_map(|it| match it {
                Effect::Stat { tag, .. } => Some(*tag),
                _ => None,
            })
            .expect("the new watch is confirmed by a stat");
        let meta = blaze_core::Meta { len: 0, dir: true, modified_ns: 0 };
        heads.complete(&mut vfs, Completion { tag: stat, done: Done::Stat(Some(meta)) }, &mut effects, &mut out);
        effects.clear();

        heads.arrive(&vfs, build(3), &mut effects, &mut out);
        let flush = effects
            .iter()
            .find_map(|it| match it {
                Effect::Flush { tag, .. } => Some(*tag),
                _ => None,
            })
            .expect("the confirmed watch is flushed");
        assert!(out.is_empty());
        assert_eq!(heads.waiting(), 1);
        heads.arrive(&vfs, Arrived::Request { from: from(4), request: Request::Status }, &mut effects, &mut out);
        assert_eq!(passed(&mut out), [4], "what runs no user code waits for no round");

        heads.complete(&mut vfs, Completion { tag: flush, done: Done::Flushed }, &mut effects, &mut out);
        assert_eq!(passed(&mut out), [3]);
        assert_eq!(heads.waiting(), 0);
    }

    #[test]
    fn what_runs_user_code_is_what_builds() {
        let view = || ViewRef::Head("ci/main".to_owned());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        assert!(runs_user_code(&Request::Check { view: view(), targets: targets.clone() }));
        assert!(runs_user_code(&Request::Verify { view: view(), targets: targets.clone() }));
        let ahead = Request::WillTest { view: view(), filter: Vec::new(), targets: targets.clone(), wait_ms: 0 };
        assert!(runs_user_code(&ahead));
        assert!(!runs_user_code(&Request::Trace { view: view(), targets }));
        assert!(!runs_user_code(&Request::Open { view: view() }));
        assert!(!runs_user_code(&Request::Status));
    }

    #[test]
    fn intent_reads_the_heads_it_builds_ahead_on_and_an_edit_reads_none() {
        let targets = || blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let view = ViewRef::Head("/w".to_owned());
        let ahead = Request::WillTest { view, filter: Vec::new(), targets: targets(), wait_ms: 0 };
        assert_eq!(named(&ahead), ["/w"]);
        let save = Request::WillSave { ops: Vec::new(), head: "/w".to_owned(), test: true, targets: targets(), wait_ms: 0 };
        assert_eq!(named(&save), ["/w"]);
        let edit = Request::Edit { base: ContentId::of("heads test", b"root"), head: "/w".to_owned(), edits: Vec::new() };
        assert!(named(&edit).is_empty());
    }

    #[test]
    fn cookies_seen_are_removed_in_number_order() {
        let mut heads = Heads::new(0, 1 << 32, 1 << 40, outside(1 << 40).1);
        let mut effects = Vec::new();
        let vfs = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]).0;
        let numbers: Vec<u64> = (0..40).map(|_| heads.cookie(&vfs, "/w", &mut effects).unwrap().1).collect();
        let checkout = heads.checkouts[0].as_deref_mut().unwrap();
        for (number, cookie) in &mut checkout.cookies {
            cookie.seen = number % 3 != 1;
        }
        effects.clear();
        heads.sweep(&mut effects);
        let removed: Vec<u64> = effects
            .iter()
            .filter_map(|it| match it {
                Effect::Remove { tag, .. } => Some((tag.0 - 1) / 2),
                _ => None,
            })
            .collect();
        let want: Vec<u64> = numbers.into_iter().filter(|it| it % 3 != 1).collect();
        assert_eq!(removed, want);
        assert_eq!(heads.checkouts[0].as_deref().unwrap().cookies.len(), 13);
    }
}
