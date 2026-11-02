use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_core::{Conn, FileName};
use blaze_ids::{ContentId, rng::SplitMix64};
use blaze_mem::{
    Tag,
    tag::{Counters, Snapshot},
};
use blaze_query::{Database, QueryKey, QueryKind};
use blaze_vfs::{Change, ChangeKind, Mode, Op, View, Vfs};

use super::{Daemon, Session};

/// The build a test's daemon says it is: a path, so a Status's copy of it allocates as the
/// daemon's does.
static BUILD: std::sync::LazyLock<crate::Build> = std::sync::LazyLock::new(|| crate::Build {
    id: ContentId([7; 32]),
    exe: crate::Executable { path: "/opt/blaze/bin/blaze".to_owned(), len: 1 << 20, modified_ns: 5, inode: 6 },
});
use crate::{
    ApiError, Arrived, Diagnostic, Edit, Edited, EditedItem, How, Level, Reach, Reply, Request,
    RequestRef, Response, Status, Target, Targets, TestOutcome, TestResult, Ub, ViewRef,
    build::{Builder, JobId, Opened, Work},
};

/// A build that records what it was asked.
#[derive(Default)]
struct Fake {
    started: Vec<(JobId, Option<Opened>, Work)>,
    cancelled: Vec<JobId>,
    speculated: Vec<(JobId, Opened, Work)>,
    promoted: Vec<JobId>,
}

impl Builder for Fake {
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work) {
        self.started.push((job, view, work));
    }

    fn cancel(&mut self, job: JobId) {
        self.cancelled.push(job);
    }

    fn speculate(&mut self, job: JobId, view: Opened, work: Work) {
        self.speculated.push((job, view, work));
    }

    fn promote(&mut self, job: JobId) {
        self.promoted.push(job);
    }
}

struct World {
    vfs: Vfs,
    db: Database,
    builder: Fake,
    session: Session,
}

impl World {
    fn new() -> World {
        let (vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        World { vfs, db: Database::empty(), builder: Fake::default(), session: Session::new() }
    }

    fn call(&mut self, f: impl FnOnce(&mut Session, &mut Daemon<'_>, &mut Vec<Reply>)) -> Vec<Reply> {
        let mut out = Vec::new();
        let mut cx = Daemon {
            vfs: &mut self.vfs,
            db: &self.db,
            builder: &mut self.builder,
            pid: 30,
            build: &BUILD,
            memory_bytes: 10,
            budget_bytes: 20,
            memory: counters,
            profile: dumped,
            hot: hotted,
            history: None,
        };
        f(&mut self.session, &mut cx, &mut out);
        out
    }

    fn request(&mut self, conn: u64, id: u64, request: Request) -> Vec<Reply> {
        let from = RequestRef { conn: Conn(conn), id };
        self.call(|session, cx, out| session.arrive(cx, Arrived::Request { from, request }, out))
    }

    fn gone(&mut self, conn: u64) -> Vec<Reply> {
        self.call(|session, cx, out| session.arrive(cx, Arrived::Gone { conn: Conn(conn) }, out))
    }

    fn report(&mut self, job: JobId, response: Response) -> Vec<Reply> {
        self.call(|session, cx, out| session.report(cx, job, response, out))
    }

    fn finish(&mut self, job: JobId, ok: bool, error: Option<ApiError>) -> Vec<Reply> {
        self.call(|session, cx, out| session.finish(cx, job, ok, error, out))
    }

    /// Every commit the session waits on made durable.
    fn durable(&mut self) -> Vec<Reply> {
        let tickets: Vec<_> = self.session.tickets.keys().copied().collect();
        self.call(|session, cx, out| session.durable(cx, &tickets, out))
    }

    fn last_job(&self) -> JobId {
        self.builder.started.last().expect("a job started").0
    }

    fn last_speculated(&self) -> JobId {
        self.builder.speculated.last().expect("a job speculated").0
    }

    fn tick(&mut self, now_ns: u64) -> Vec<Reply> {
        self.call(|session, cx, _| session.tick(cx, now_ns))
    }

    /// `main` with `files` written, made durable.
    fn commit(&mut self, files: &[(&str, &str)]) -> ContentId {
        let base = self.vfs.head("main").unwrap_or_else(|| self.vfs.empty());
        let ops = files
            .iter()
            .map(|(path, bytes)| write(path, bytes))
            .collect();
        assert_eq!(self.request(9, 9, Request::Commit { head: "main".to_owned(), base, ops }), []);
        let replies = self.durable();
        assert!(matches!(replies[1].response, Response::Done { ok: true }), "{replies:?}");
        self.vfs.head("main").unwrap()
    }
}

fn targets() -> Targets {
    Targets {
        packages: Vec::new(),
        profile: "dev".to_owned(),
        target: None,
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
        cargo: None,
    }
}

fn head(name: &str) -> ViewRef {
    ViewRef::Head(name.to_owned())
}

fn root_of(root: ContentId) -> ViewRef {
    ViewRef::Root(View { root })
}

fn to(conn: u64, id: u64) -> RequestRef {
    RequestRef { conn: Conn(conn), id }
}

fn reply(conn: u64, id: u64, response: Response) -> Reply {
    Reply { to: to(conn, id), response }
}

fn done(conn: u64, id: u64, ok: bool) -> Reply {
    reply(conn, id, Response::Done { ok })
}

fn diagnostic(level: Level) -> Response {
    Response::Diagnostic(Diagnostic {
        level,
        code: None,
        message: "a diagnostic".to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: "a diagnostic".to_owned(),
        unit: None,
        explanation: None,
    })
}

fn test_result(outcome: TestOutcome) -> Response {
    Response::TestResult(TestResult {
        binary: "tests".to_owned(),
        name: "a_test".to_owned(),
        cached: None,
        outcome,
        output: Vec::new(),
        millis: 1,
        outside: false,
        executable: 0,
    })
}

fn of(candidate: u32, response: Response) -> Response {
    Response::Of { candidate, response: Box::new(response) }
}

fn error_at(replies: &[Reply]) -> Option<String> {
    replies.iter().find_map(|it| match &it.response {
        Response::Error(error) => error.at.clone(),
        _ => None,
    })
}

/// The candidate of an `Of` error, and where the error was.
fn of_error_at(reply: &Reply) -> Option<(u32, Option<String>)> {
    let Response::Of { candidate, response } = &reply.response else { return None };
    let Response::Error(error) = &**response else { return None };
    Some((*candidate, error.at.clone()))
}

fn write(path: &str, bytes: &str) -> Op {
    Op::Write { path: path.to_owned(), bytes: bytes.as_bytes().to_vec(), mode: Mode::Regular }
}

#[test]
fn a_check_forwards_its_reports_then_ends_and_closes_its_view() {
    let mut world = World::new();
    assert_eq!(world.request(1, 1, Request::Check { view: head("main"), targets: targets() }), []);
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!(view.unwrap().view, View { root: world.vfs.empty() }, "a head never written is empty");
    assert_eq!(work, Work::Check(targets()));
    assert_eq!(world.session.views(), 1);

    let warning = diagnostic(Level::Warning);
    assert_eq!(world.report(job, warning.clone()), [reply(1, 1, warning)]);
    assert_eq!(world.finish(job, false, None), [done(1, 1, false)]);
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_job_that_could_not_run_ends_with_its_error() {
    let mut world = World::new();
    world.request(1, 1, Request::Build { view: head("main"), targets: targets() });
    let job = world.last_job();
    let error = ApiError::new("no package `x`", Some("x".to_owned()));
    assert_eq!(
        world.finish(job, true, Some(error.clone())),
        [reply(1, 1, Response::Error(error)), done(1, 1, false)]
    );
}

#[test]
fn test_run_and_verify_start_their_work() {
    let mut world = World::new();
    let filter = vec!["parse".to_owned()];
    let args = vec!["--exact".to_owned()];
    let ub = Some(Ub { seed: Some(7), schedules: None });
    world.request(
        1,
        1,
        Request::Test { view: head("main"), targets: targets(), filter: filter.clone(), all: true, args: args.clone(), ub },
    );
    world.request(1, 2, Request::Run { view: head("main"), targets: targets(), bin: None, args: vec!["-v".to_owned()] });
    world.request(1, 3, Request::Verify { view: head("main"), targets: targets() });
    let works: Vec<Work> = world.builder.started.iter().map(|it| it.2.clone()).collect();
    assert_eq!(
        works,
        [
            Work::Test { targets: targets(), filter, all: true, args, ub },
            Work::Run { targets: targets(), bin: None, args: vec!["-v".to_owned()] },
            Work::Verify(targets()),
        ]
    );
}

#[test]
fn the_proofs_requests_start_their_work_on_the_view() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    world.request(1, 1, Request::CodegenInputs { view: head("main"), targets: targets() });
    world.request(1, 2, Request::Trace { view: head("main"), targets: targets() });
    world.request(1, 3, Request::Tables { view: head("main") });
    let started: Vec<(Option<View>, Work)> = world
        .builder
        .started
        .iter()
        .map(|(_, opened, work)| (opened.map(|opened| opened.view), work.clone()))
        .collect();
    let view = Some(View { root });
    assert_eq!(
        started,
        [
            (view, Work::CodegenInputs(targets())),
            (view, Work::Trace(targets())),
            (view, Work::Tables),
        ]
    );
}

#[test]
fn a_commit_is_answered_only_once_durable() {
    let mut world = World::new();
    let base = world.vfs.empty();
    let commit = Request::Commit { head: "main".to_owned(), base, ops: vec![write("src/lib.rs", "")] };
    assert_eq!(world.request(1, 1, commit), []);
    let root = world.vfs.head("main").unwrap();
    assert_eq!(world.call(|session, cx, out| session.durable(cx, &[], out)), []);
    assert_eq!(
        world.durable(),
        [reply(1, 1, Response::Committed { root, rebased: false }), done(1, 1, true)]
    );
    assert_eq!(world.durable(), [], "answered once");
}

#[test]
fn a_commit_on_an_unknown_base_names_the_head() {
    let mut world = World::new();
    let base = ContentId([7; 32]);
    let replies = world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops: Vec::new() });
    assert_eq!(error_at(&replies), Some("main".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 1, false)));
}

#[test]
fn a_commit_whose_op_fails_names_the_path() {
    let mut world = World::new();
    let base = world.vfs.empty();
    let ops = vec![Op::Delete { path: "gone.rs".to_owned() }];
    let replies = world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops });
    assert_eq!(error_at(&replies), Some("gone.rs".to_owned()));
    assert_eq!(replies.len(), 2);
}

#[test]
fn a_what_if_resolves_inside_out_and_names_a_path_that_fails() {
    let mut world = World::new();
    let inner = ViewRef::WhatIf { base: Box::new(head("main")), ops: vec![write("a.rs", "a")] };
    let outer = ViewRef::WhatIf { base: Box::new(inner.clone()), ops: vec![write("b.rs", "b")] };
    world.request(1, 1, Request::Check { view: outer, targets: targets() });
    let root = world.builder.started[0].1.unwrap().view.root;
    let changes = world.vfs.diff(world.vfs.empty(), root);
    let paths: Vec<&str> = changes.iter().map(|it| it.path.as_str()).collect();
    assert_eq!(paths, ["a.rs", "b.rs"]);
    assert_eq!(world.vfs.head("main"), None, "a what-if moves no head");

    let bad = ViewRef::WhatIf { base: Box::new(inner), ops: vec![Op::Delete { path: "c.rs".to_owned() }] };
    let replies = world.request(1, 2, Request::Check { view: bad, targets: targets() });
    assert_eq!(error_at(&replies), Some("c.rs".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    assert_eq!(world.builder.started.len(), 1);
}

#[test]
fn an_unknown_root_is_an_error_naming_it() {
    let mut world = World::new();
    let root = ContentId([3; 32]);
    let replies = world.request(1, 1, Request::Open { view: ViewRef::Root(View { root }) });
    assert_eq!(error_at(&replies), Some(format!("root {root:x}")));
}

#[test]
fn opens_are_counted_and_each_needs_a_close() {
    let mut world = World::new();
    let root = world.vfs.empty();
    assert_eq!(world.request(1, 1, Request::Open { view: head("main") }), [reply(1, 1, Response::Opened { root }), done(1, 1, true)]);
    world.request(1, 2, Request::Open { view: ViewRef::Root(View { root }) });
    assert_eq!(world.session.views(), 2);
    assert_eq!(world.request(2, 1, Request::Close { root }).last(), Some(&done(2, 1, false)), "another connection's");
    assert_eq!(world.request(1, 3, Request::Close { root }), [done(1, 3, true)]);
    assert_eq!(world.request(1, 4, Request::Close { root }), [done(1, 4, true)]);
    let replies = world.request(1, 5, Request::Close { root });
    assert_eq!(error_at(&replies), Some(format!("root {root:x}")));
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_gone_connection_closes_every_view_it_holds_of_every_root_and_no_other() {
    let mut world = World::new();
    let empty = world.vfs.empty();
    let other = world.commit(&[("src/lib.rs", "fn f() {}")]);
    for id in 1..=3 {
        world.request(1, id, Request::Open { view: ViewRef::Root(View { root: empty }) });
        world.request(1, 10 + id, Request::Open { view: ViewRef::Root(View { root: other }) });
    }
    world.request(2, 1, Request::Open { view: ViewRef::Root(View { root: other }) });
    assert_eq!(world.session.views(), 7);
    assert_eq!(world.gone(1), []);
    assert_eq!(world.session.views(), 1);
    assert_eq!(world.request(2, 2, Request::Close { root: other }), [done(2, 2, true)]);
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_diff_lists_the_changes() {
    let mut world = World::new();
    world.commit(&[("src/lib.rs", "fn f() {}")]);
    let replies = world.request(1, 1, Request::Diff { from: head("empty"), to: head("main") });
    let changes = vec![Change { path: "src/lib.rs".to_owned(), kind: ChangeKind::Added }];
    assert_eq!(replies, [reply(1, 1, Response::Diff { changes }), done(1, 1, true)]);
}

#[test]
fn why_reads_the_last_build_of_the_root_and_names_a_query_that_did_not_rerun() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    let query = QueryKey { kind: QueryKind(0x2001), input: ContentId([1; 32]) };
    let replies = world.request(1, 1, Request::Why { view: head("main"), query });
    assert_eq!(error_at(&replies), Some(format!("root {root:x}")));
    assert_eq!(replies[1], done(1, 1, false));

    world.request(1, 2, Request::Check { view: head("main"), targets: targets() });
    world.finish(world.last_job(), true, None);
    let replies = world.request(1, 3, Request::Why { view: head("main"), query });
    assert_eq!(error_at(&replies), Some(super::query_name(&query)));
    let Response::Error(error) = &replies[0].response else { panic!("{replies:?}") };
    assert!(error.what.contains("did not rerun in the last build of"), "{}", error.what);
    assert_eq!(replies[1], done(1, 3, false));
}

#[test]
fn a_reach_keeps_no_build_for_why() {
    let mut world = World::new();
    let from = world.commit(&[("src/lib.rs", "")]);
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    world.finish(world.last_job(), true, None);
    let to = world.commit(&[("src/lib.rs", "fn f() {}")]);
    let reach = Request::Reach { from: root_of(from), to: root_of(to), targets: targets() };
    world.request(1, 2, reach);
    world.finish(world.last_job(), true, None);
    let query = QueryKey { kind: QueryKind(0x2001), input: ContentId([1; 32]) };
    let replies = world.request(1, 3, Request::Why { view: root_of(from), query });
    assert_eq!(error_at(&replies), Some(super::query_name(&query)));
    let replies = world.request(1, 4, Request::Why { view: root_of(to), query });
    assert_eq!(error_at(&replies), Some(format!("root {to:x}")));
}

#[test]
fn why_keeps_the_view_a_build_ran_in_when_the_daemon_hands_it_over() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    let built = world.db.open(View { root });
    let replies =
        world.call(|session, cx, out| session.finish_in(cx, job, true, None, Some(built), out));
    assert_eq!(replies, [done(1, 1, true)]);
    assert_eq!(world.session.kept.iter().copied().collect::<Vec<_>>(), [(root, built)]);

    world.request(1, 2, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    let failed = world.db.open(View { root });
    let error = Some(ApiError::new("no".to_owned(), None));
    world.call(|session, cx, out| session.finish_in(cx, job, false, error, Some(failed), out));
    assert_eq!(
        world.session.kept.iter().copied().collect::<Vec<_>>(),
        [(root, built)],
        "a job that could not run keeps nothing: its handed view is closed"
    );
}

#[test]
fn stop_is_answered_done_at_once() {
    let mut world = World::new();
    assert_eq!(world.request(1, 1, Request::Stop), [done(1, 1, true)]);
}

#[test]
fn status_lists_every_connections_requests_sorted() {
    let mut world = World::new();
    world.request(2, 8, Request::Check { view: head("main"), targets: targets() });
    world.request(1, 5, Request::Check { view: head("main"), targets: targets() });
    world.request(1, 1, Request::Open { view: head("main") });
    let status =
        Status { pid: 30, requests: vec![5, 8], views: 3, history: None, memory_bytes: 10, budget_bytes: 20, build: BUILD.clone() };
    assert_eq!(world.request(3, 1, Request::Status), [reply(3, 1, Response::Status(status)), done(3, 1, true)]);
}

/// A warm Status, with one build under way, allocates only its reply's own: the one allocation
/// is the `requests` list of [`Status`], which goes out in the reply. The session's sort,
/// its views' count and the replies' pushes into the daemon's kept `out` allocate nothing.
#[test]
fn a_warm_status_allocates_only_its_replys_list_of_requests() {
    let mut world = World::new();
    world.request(1, 1, Request::Build { view: head("main"), targets: targets() });
    let mut out = Vec::with_capacity(4);
    for id in 0..8u64 {
        out.clear();
        let World { vfs, db, builder, session } = &mut world;
        let mut cx = Daemon {
            vfs,
            db: &*db,
            builder,
            pid: 30,
            build: &BUILD,
            memory_bytes: 10,
            budget_bytes: 20,
            memory: counters,
            profile: dumped,
            hot: hotted,
            history: None,
        };
        let from = to(2, id);
        let arrived = Arrived::Request { from, request: Request::Status };
        blaze_alloc_count::assert_at_most(2, "a warm Status: its reply's list of requests and its executable's path", || {
            session.arrive(&mut cx, arrived, &mut out)
        });
        let status = Status {
            pid: 30,
            requests: vec![1],
            views: 1,
            history: None,
            memory_bytes: 10,
            budget_bytes: 20,
            build: BUILD.clone(),
        };
        assert_eq!(out, [reply(2, id, Response::Status(status)), done(2, id, true)]);
    }
}

/// The flight recorder's dump as a test's daemon has it: the window's seconds, as text.
fn dumped(window: std::time::Duration) -> Vec<u8> {
    window.as_secs().to_string().into_bytes()
}

/// The live sketches as a test's daemon has them: one key whose time is `top` ms since the
/// start, and no last build.
fn hotted(top: u32, since: crate::HotSince) -> Result<crate::Hot, ApiError> {
    use crate::{Hot, HotFunction, HotKey, HotSince, HotWait, HotWaitKind};
    if since == HotSince::LastBuild {
        return Err(ApiError::new("no build has ended in this daemon", Some("blaze debug hot --last-build".to_owned())));
    }
    let ns = u64::from(top) * 1_000_000;
    Ok(Hot {
        keys: vec![HotKey {
            span: "infer_query".to_owned(),
            function: HotFunction::Named("app::main".to_owned()),
            lower_ns: ns / 2,
            upper_ns: ns,
        }],
        torn: 0,
        since,
        spans: 1,
        waits: vec![HotWait { kind: HotWaitKind::Key, count: 1, max_ns: 9, p50_ns: 9, p90_ns: 9, p99_ns: 9 }],
        error_ns: 0,
        total_ns: ns,
    })
}

/// A hot request is answered with the daemon's reading of its sketches, and its refusal (no
/// last build yet) is the request's error.
#[test]
fn hot_is_the_daemons_sketches_and_their_refusal_is_its_error() {
    let mut world = World::new();
    let hot = world.request(1, 1, Request::Hot { top: 20, since: crate::HotSince::Start });
    let Some(Reply { response: Response::Hot(hot), .. }) = hot.first() else { panic!("{hot:?}") };
    assert_eq!((hot.keys.len(), hot.keys[0].upper_ns, hot.since), (1, 20_000_000, crate::HotSince::Start));
    let refused = world.request(1, 2, Request::Hot { top: 20, since: crate::HotSince::LastBuild });
    assert_eq!(error_at(&refused).as_deref(), Some("blaze debug hot --last-build"), "{refused:?}");
    assert_eq!(refused.last(), Some(&done(1, 2, false)));
}

/// How many times [`counters`] was read: the parse tag hands out 3 MB between reads.
static READS: AtomicU64 = AtomicU64::new(0);

/// blaze-mem's counters as a test's daemon has them: every tag's numbers its own, `infer`
/// over a budget of 4 KiB, and `parse` handing out 3 MB a read.
fn counters() -> Snapshot {
    let reads = READS.fetch_add(1, Ordering::Relaxed);
    let mut tags = [Counters::default(); Tag::COUNT];
    for (at, it) in tags.iter_mut().enumerate() {
        let at = at as u64;
        *it = Counters {
            live: 10 + at,
            high_water: 20 + at,
            allocated: 30 + at,
            task_peak: 40 + at,
            grow_copies: 0,
            grow_copied: 0,
            budget: u64::MAX,
        };
    }
    tags[Tag::Infer.index()].budget = 4096;
    tags[Tag::Parse.index()].allocated = 3_000_000 * reads;
    Snapshot { tags, committed: 1 << 20, committed_high_water: 1 << 21 }
}

/// A profile request is answered with the daemon's dump of the seconds it asks for; the
/// explorer, the live feed and live SQL are the daemon loop's, so one that reaches the session
/// fails naming its command.
#[test]
fn a_profile_is_the_daemons_dump_and_the_loops_requests_are_refused_by_name() {
    let mut world = World::new();
    let profiled = world.request(1, 1, Request::Profile { seconds: 60 });
    assert_eq!(profiled, [reply(1, 1, Response::Profiled { trace: b"60".to_vec() }), done(1, 1, true)]);
    for (id, request, named) in [
        (2, Request::Explore { dev: false }, "blaze explore"),
        (3, Request::Live, "blaze debug live"),
        (4, Request::Sql { sql: "SELECT 1".to_owned(), last_s: 30 }, "blaze debug sql --live"),
    ] {
        let refused = world.request(1, id, request);
        assert_eq!(error_at(&refused).as_deref(), Some(named), "{refused:?}");
        assert_eq!(refused.last(), Some(&done(1, id, false)));
    }
}

/// A memory request is answered with one record per tag, in blaze-mem's order, each the
/// counters' numbers, and the committed bytes; the rate is what a tag handed out a second
/// between two requests on the session's clock, 0 for the first.
#[test]
fn memory_gives_each_subsystems_counters_and_rate() {
    let mut world = World::new();
    world.tick(1_000_000_000);
    let first = world.request(1, 1, Request::Memory);
    let [Reply { response: Response::Memory(memory), .. }, end] = &first[..] else {
        panic!("one memory record, then the end: {first:?}");
    };
    assert_eq!(end, &done(1, 1, true));
    assert_eq!((memory.committed, memory.committed_high_water), (1 << 20, 1 << 21));
    let names: Vec<&str> = memory.tags.iter().map(|it| it.tag.as_str()).collect();
    let want: Vec<&str> = Tag::ALL.iter().map(|it| it.name()).collect();
    assert_eq!(names, want);
    let io = &memory.tags[Tag::Io.index()];
    let at = Tag::Io.index() as u64;
    assert_eq!(
        (io.live, io.high_water, io.allocated, io.task_peak, io.budget, io.rate),
        (10 + at, 20 + at, 30 + at, 40 + at, u64::MAX, 0)
    );
    assert_eq!(memory.tags[Tag::Infer.index()].budget, 4096);
    assert!(memory.tags.iter().all(|it| it.rate == 0), "no rate before a second sample");

    world.tick(3_000_000_000);
    let second = world.request(1, 2, Request::Memory);
    let [Reply { response: Response::Memory(memory), .. }, _] = &second[..] else {
        panic!("one memory record, then the end: {second:?}");
    };
    let parse = &memory.tags[Tag::Parse.index()];
    assert_eq!(parse.rate, 1_500_000, "3 MB over 2 s");
    assert!(memory.tags.iter().filter(|it| it.tag != "parse").all(|it| it.rate == 0));
}

#[test]
fn a_reach_diffs_the_views_and_runs_on_the_later() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    world.request(1, 1, Request::Reach { from: head("empty"), to: head("main"), targets: targets() });
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!(view.unwrap().view.root, root);
    let Work::Reach { from, changes, .. } = work else { panic!("{work:?}") };
    assert_eq!(from.view.root, world.vfs.empty());
    assert_eq!(changes, [Change { path: "src/lib.rs".to_owned(), kind: ChangeKind::Added }]);
    assert_eq!(world.session.views(), 2);

    let reached = Response::Reached(Reach::default());
    assert_eq!(world.report(job, reached.clone()), [reply(1, 1, reached)]);
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_snapshot_has_no_view() {
    let mut world = World::new();
    world.request(1, 1, Request::Snapshot);
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!((view, work), (None, Work::Snapshot));
    let snapshotted = Response::Snapshotted { heads: Vec::new() };
    assert_eq!(world.report(job, snapshotted.clone()), [reply(1, 1, snapshotted)]);
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
}

#[test]
fn a_cancel_cancels_the_jobs_and_the_request_still_ends_once() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    assert_eq!(world.request(1, 2, Request::Cancel { id: 1 }), [done(1, 2, true)]);
    assert_eq!(world.request(1, 3, Request::Cancel { id: 1 }), [done(1, 3, true)]);
    assert_eq!(world.builder.cancelled, [job], "cancelled once");
    assert_eq!(world.finish(job, false, None), [done(1, 1, false)]);

    let replies = world.request(1, 4, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()), "it is over");
    let replies = world.request(2, 5, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()), "another connection's");
}

#[test]
fn a_commit_waiting_to_be_durable_cant_be_cancelled() {
    let mut world = World::new();
    let base = world.vfs.empty();
    world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops: vec![write("a", "")] });
    let replies = world.request(1, 2, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    let replies = world.durable();
    assert_eq!(replies.last(), Some(&done(1, 1, true)));
}

#[test]
fn a_gone_connection_is_cancelled_closed_and_sent_nothing() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    world.request(1, 2, Request::Open { view: head("main") });
    let base = world.vfs.empty();
    world.request(1, 3, Request::Commit { head: "main".to_owned(), base, ops: vec![write("a", "")] });
    world.request(2, 1, Request::Snapshot);
    let other = world.last_job();
    assert_eq!(world.session.views(), 2);

    assert_eq!(world.gone(1), []);
    assert_eq!(world.builder.cancelled, [job]);
    assert_eq!(world.session.views(), 1, "the job's view stays until it finishes");
    assert_eq!(world.report(job, diagnostic(Level::Error)), []);
    assert_eq!(world.finish(job, false, None), []);
    assert_eq!(world.durable(), []);
    assert_eq!(world.session.views(), 0);
    assert_eq!(world.session.pending.len(), 1, "only connection 2's snapshot");

    assert_eq!(world.finish(other, true, None), [done(2, 1, true)]);
    assert!(world.session.pending.is_empty());
}

#[test]
fn a_duplicate_id_is_refused_without_touching_the_first() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    let replies = world.request(1, 1, Request::Snapshot);
    assert_eq!(error_at(&replies), Some("request 1".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 1, false)));
    assert_eq!(world.builder.started.len(), 1);
    assert_eq!(world.request(2, 1, Request::Snapshot).len(), 0, "another connection may use it");
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
}

fn worked() -> Response {
    Response::Work(crate::Worked {
        reran: 3,
        green: 9,
        served: Vec::new(),
        compiled: 1,
        found: 4,
        link: crate::LinkKind::Placed,
        links: vec![crate::Linked { how: crate::LinkHow::First, path: "target/debug/app".to_owned() }],
        critical_path_us: 70,
        steps_us: vec![(crate::NodeKind::Codegen, 70)],
    })
}

#[test]
fn a_build_forwards_its_work_before_done() {
    let mut world = World::new();
    world.request(1, 1, Request::Build { view: head("main"), targets: targets() });
    let job = world.last_job();
    assert_eq!(world.report(job, worked()), [reply(1, 1, worked())]);
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
}

#[test]
fn a_build_test_or_run_that_ends_without_its_work_fails_naming_the_job() {
    let mut world = World::new();
    let requests = [
        Request::Build { view: head("main"), targets: targets() },
        Request::Test { view: head("main"), targets: targets(), filter: Vec::new(), all: false, args: Vec::new(), ub: None },
        Request::Run { view: head("main"), targets: targets(), bin: None, args: Vec::new() },
    ];
    for (id, request) in (1..).zip(requests) {
        world.request(1, id, request);
        let job = world.last_job();
        let replies = world.finish(job, true, None);
        assert_eq!(error_at(&replies), Some(format!("job {job}")));
        assert_eq!(replies.last(), Some(&done(1, id, false)));
    }
}

#[test]
fn a_check_or_a_cancelled_build_owes_no_work() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    assert_eq!(world.finish(world.last_job(), true, None), [done(1, 1, true)]);
    world.request(1, 2, Request::Build { view: head("main"), targets: targets() });
    let job = world.last_job();
    world.request(1, 3, Request::Cancel { id: 2 });
    assert_eq!(world.finish(job, false, None), [done(1, 2, false)]);
}

#[test]
fn a_report_or_finish_after_the_finish_is_dropped() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    world.finish(job, true, None);
    assert_eq!(world.report(job, diagnostic(Level::Error)), []);
    assert_eq!(world.finish(job, false, None), []);
    let unknown = JobId { slot: job.slot + 99, ..job };
    assert_eq!(world.report(unknown, diagnostic(Level::Error)), [], "a slot never made");
    assert_eq!(world.finish(unknown, true, None), []);
    let free = JobId { generation: job.generation + 1, ..job };
    assert_eq!(world.report(free, diagnostic(Level::Error)), [], "the free slot's generation");
    assert_eq!(world.finish(free, true, None), []);
}

#[test]
fn a_finished_jobs_slot_is_reused_and_its_old_id_refused() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let old = world.last_job();
    assert_eq!(world.finish(old, true, None), [done(1, 1, true)]);
    world.request(1, 2, Request::Check { view: head("main"), targets: targets() });
    let new = world.last_job();
    assert_eq!(new.slot, old.slot, "the freed slot, before a new one");
    assert_ne!(new, old);

    assert_eq!(world.report(old, diagnostic(Level::Error)), [], "a report naming the old id");
    assert_eq!(world.finish(old, false, None), [], "a finish naming the old id");
    assert_eq!(world.session.views(), 1, "the new job still holds its view");
    let warning = diagnostic(Level::Warning);
    assert_eq!(world.report(new, warning.clone()), [reply(1, 2, warning)]);
    assert_eq!(world.finish(new, true, None), [done(1, 2, true)]);

    for id in 3..40 {
        world.request(1, id, Request::Snapshot);
        if id % 3 != 0 {
            world.finish(world.last_job(), true, None);
        }
    }
    let jobs: BTreeSet<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    assert_eq!(jobs.len(), world.builder.started.len(), "no id handed out twice");
}

/// Intents are taken over in the order their jobs started, though reused slots put their ids
/// out of that order.
#[test]
fn intents_are_taken_over_in_start_order_after_slots_are_reused() {
    let mut world = World::new();
    world.commit(&[("src/lib.rs", "")]);
    for id in 1..=3 {
        world.request(5, id, Request::Snapshot);
    }
    let snapshots: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    world.finish(snapshots[0], true, None);
    world.finish(snapshots[1], true, None);

    for conn in 1..=3 {
        let will = Request::WillTest { view: head("main"), filter: Vec::new(), targets: targets(), wait_ms: 100 };
        world.request(conn, 1, will);
    }
    let intents: Vec<JobId> = world.builder.speculated.iter().map(|it| it.0).collect();
    let mut by_id = intents.clone();
    by_id.sort_unstable();
    assert_ne!(intents, by_id, "reused slots put the ids out of start order");

    world.request(4, 1, Request::Check { view: head("main"), targets: targets() });
    assert_eq!(world.builder.promoted, intents, "taken over in start order");
    for job in intents {
        assert_eq!(world.finish(job, true, None), []);
    }
    let check = world.last_job();
    assert_eq!(world.finish(check, true, None), [done(4, 1, true)]);
    assert_eq!(world.finish(snapshots[2], true, None), [done(5, 3, true)]);
    assert_eq!(world.session.views(), 0);
}

/// A warm start and finish of a job allocates nothing: its id is a slot of the session's table
/// of jobs, freed and reused, and the request's own entries go back to maps that keep their room.
#[test]
fn a_warm_start_and_finish_of_a_job_allocates_nothing() {
    let mut world = World::new();
    world.request(1, 0, Request::Snapshot);
    world.finish(world.last_job(), true, None);
    world.builder.started.reserve(16);
    let mut out = Vec::with_capacity(4);
    for id in 1..8u64 {
        out.clear();
        let World { vfs, db, builder, session } = &mut world;
        let mut cx = Daemon {
            vfs: &mut *vfs,
            db: &*db,
            builder: &mut *builder,
            pid: 30,
            build: &BUILD,
            memory_bytes: 10,
            budget_bytes: 20,
            memory: counters,
            profile: dumped,
            hot: hotted,
            history: None,
        };
        let arrived = Arrived::Request { from: to(1, id), request: Request::Snapshot };
        blaze_alloc_count::assert_at_most(0, "a warm start of a job", || {
            session.arrive(&mut cx, arrived, &mut out)
        });
        let job = builder.started.last().expect("a job started").0;
        let mut cx = Daemon {
            vfs: &mut *vfs,
            db: &*db,
            builder: &mut *builder,
            pid: 30,
            build: &BUILD,
            memory_bytes: 10,
            budget_bytes: 20,
            memory: counters,
            profile: dumped,
            hot: hotted,
            history: None,
        };
        blaze_alloc_count::assert_at_most(0, "a warm finish of a job", || {
            session.finish(&mut cx, job, true, None, &mut out)
        });
        assert_eq!(out, [done(1, id, true)]);
    }
    let slots: BTreeSet<u32> = world.builder.started.iter().map(|it| it.0.slot).collect();
    assert_eq!(slots.len(), 1, "one slot, reused");
}

#[test]
fn best_of_ranks_with_ties_and_an_unappliable_candidate_last() {
    let mut world = World::new();
    let candidates = vec![
        vec![write("a.rs", "a")],
        vec![Op::Delete { path: "missing.rs".to_owned() }],
        vec![write("b.rs", "b")],
        vec![write("c.rs", "c")],
    ];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() };
    let replies = world.request(1, 1, request);
    assert_eq!(replies.len(), 2);
    assert_eq!(of_error_at(&replies[0]), Some((1, Some("missing.rs".to_owned()))));
    assert_eq!(replies[1], reply(1, 1, of(1, Response::Done { ok: false })));
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    assert_eq!(jobs.len(), 3, "the rest still run");
    assert!(world.builder.started.iter().all(|it| it.2 == Work::Check(targets())));
    assert_eq!(world.session.views(), 3);

    assert_eq!(world.report(jobs[0], diagnostic(Level::Error)), [reply(1, 1, of(0, diagnostic(Level::Error)))]);
    world.report(jobs[1], diagnostic(Level::Warning));
    world.report(jobs[2], diagnostic(Level::Warning));
    world.report(jobs[2], diagnostic(Level::Note));
    assert_eq!(world.finish(jobs[0], false, None), [reply(1, 1, of(0, Response::Done { ok: false }))]);
    world.finish(jobs[2], true, None);
    let replies = world.finish(jobs[1], true, None);
    assert_eq!(replies.len(), 3);
    let Response::Ranked(ranked) = &replies[1].response else { panic!("{replies:?}") };
    let order: Vec<u32> = ranked.iter().map(|it| it.candidate).collect();
    assert_eq!(order, [2, 3, 0, 1], "a tie goes to the earlier candidate");
    assert_eq!((ranked[0].warnings, ranked[2].errors), (1, 1));
    assert_eq!(ranked[3].root, world.vfs.empty(), "the unappliable has the base's root");
    assert!(!ranked[3].ok && ranked[0].ok);
    assert_eq!(replies[2], done(1, 1, true));
    assert_eq!(world.session.views(), 0);
}

#[test]
fn best_of_tests_rank_by_failed_then_passed() {
    let mut world = World::new();
    let candidates = vec![vec![write("a.rs", "a")], vec![write("b.rs", "b")], vec![write("c.rs", "c")]];
    let filter = vec!["it".to_owned()];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: true, filter: filter.clone() };
    assert_eq!(world.request(1, 1, request), []);
    assert!(world.builder.started.iter().all(|it| it.2 == Work::Test { targets: targets(), filter: filter.clone(), all: false, args: Vec::new(), ub: None }));
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    world.report(jobs[0], test_result(TestOutcome::Passed));
    world.report(jobs[0], test_result(TestOutcome::Failed));
    world.report(jobs[1], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Unreached));
    world.finish(jobs[0], false, None);
    world.finish(jobs[1], true, None);
    let error = ApiError::new("the sandbox could not start", Some("tests".to_owned()));
    let replies = world.finish(jobs[2], false, Some(error.clone()));
    assert_eq!(replies[0], reply(1, 1, of(2, Response::Error(error))));
    let Response::Ranked(ranked) = &replies[2].response else { panic!("{replies:?}") };
    let order: Vec<u32> = ranked.iter().map(|it| it.candidate).collect();
    assert_eq!(order, [1, 0, 2], "one that could not run goes last");
    assert_eq!(replies[3], done(1, 1, true));
}

#[test]
fn best_of_with_no_candidate_that_applies_ends_at_once() {
    let mut world = World::new();
    let candidates = vec![vec![Op::Delete { path: "x".to_owned() }]];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() };
    let replies = world.request(1, 1, request);
    assert!(matches!(replies[2].response, Response::Ranked(ref it) if it.len() == 1));
    assert_eq!(replies[3], done(1, 1, false));
    assert!(world.session.pending.is_empty());
    let replies = world.request(1, 2, Request::BestOf { base: head("main"), candidates: Vec::new(), targets: targets(), test: false, filter: Vec::new() });
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    assert!(error_at(&replies).is_some());
}

#[test]
fn a_best_of_cancel_cancels_every_running_candidate() {
    let mut world = World::new();
    let candidates = vec![vec![write("a.rs", "a")], vec![write("b.rs", "b")]];
    world.request(1, 1, Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() });
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    world.finish(jobs[0], true, None);
    world.request(1, 2, Request::Cancel { id: 1 });
    assert_eq!(world.builder.cancelled, [jobs[1]]);
    let replies = world.finish(jobs[1], false, None);
    assert_eq!(replies.last(), Some(&done(1, 1, true)));
}

/// What the replies so far say, checked as they come.
#[derive(Default)]
struct Seen {
    asked: BTreeSet<RequestRef>,
    done: BTreeSet<RequestRef>,
    gone: BTreeSet<Conn>,
}

impl Seen {
    fn take(&mut self, replies: Vec<Reply>) {
        for reply in replies {
            assert!(!self.gone.contains(&reply.to.conn), "a reply to a gone connection: {reply:?}");
            assert!(self.asked.contains(&reply.to), "a reply to no request: {reply:?}");
            assert!(!self.done.contains(&reply.to), "a reply after the Done: {reply:?}");
            if let Response::Done { .. } = reply.response {
                self.done.insert(reply.to);
            }
        }
    }
}

#[test]
fn random_interleavings_end_each_request_once_and_never_answer_a_gone_connection() {
    for seed in 1..=200u64 {
        let mut rng = SplitMix64::new(seed);
        let mut world = World::new();
        let mut seen = Seen::default();
        let mut conns: Vec<u64> = vec![0, 1, 2];
        let mut next_conn = 3;
        let mut ids: BTreeMap<u64, u64> = BTreeMap::new();
        let mut finished: BTreeSet<JobId> = BTreeSet::new();
        let mut written = 0;

        for _ in 0..200 {
            match rng.index(8) {
                0..=2 if !conns.is_empty() => {
                    let conn = conns[rng.index(conns.len())];
                    let next = ids.entry(conn).or_insert(0);
                    let id = *next;
                    *next += 1;
                    let request = match rng.index(7) {
                        0 => Request::Check { view: head("main"), targets: targets() },
                        1 => Request::Snapshot,
                        2 => Request::BestOf {
                            base: head("main"),
                            candidates: vec![
                                vec![write("x.rs", "x")],
                                vec![Op::Delete { path: "never.rs".to_owned() }],
                                vec![write("y.rs", "y")],
                            ],
                            targets: targets(),
                            test: rng.index(2) == 0,
                            filter: Vec::new(),
                        },
                        3 => {
                            written += 1;
                            let base = world.vfs.head("main").unwrap_or_else(|| world.vfs.empty());
                            let ops = vec![write(&format!("f{written}.rs"), "")];
                            Request::Commit { head: "main".to_owned(), base, ops }
                        }
                        4 if id > 0 => Request::Cancel { id: rng.index(id as usize) as u64 },
                        5 => Request::Open { view: head("main") },
                        _ => Request::Status,
                    };
                    seen.asked.insert(to(conn, id));
                    let replies = world.request(conn, id, request);
                    seen.take(replies);
                }
                3 | 4 if !world.builder.started.is_empty() => {
                    let job = world.builder.started[rng.index(world.builder.started.len())].0;
                    let response = if rng.index(2) == 0 {
                        diagnostic(Level::Error)
                    } else {
                        test_result(TestOutcome::Passed)
                    };
                    let replies = world.report(job, response);
                    seen.take(replies);
                }
                5 if !world.builder.started.is_empty() => {
                    let job = world.builder.started[rng.index(world.builder.started.len())].0;
                    finished.insert(job);
                    let replies = world.finish(job, rng.index(2) == 0, None);
                    seen.take(replies);
                }
                6 => {
                    let replies = world.durable();
                    seen.take(replies);
                }
                7 if !conns.is_empty() && rng.index(4) == 0 => {
                    let conn = conns.swap_remove(rng.index(conns.len()));
                    seen.gone.insert(Conn(conn));
                    let replies = world.gone(conn);
                    seen.take(replies);
                    conns.push(next_conn);
                    next_conn += 1;
                }
                _ => {}
            }
        }

        let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
        for job in jobs.into_iter().filter(|it| !finished.contains(it)) {
            let replies = world.finish(job, true, None);
            seen.take(replies);
        }
        let replies = world.durable();
        seen.take(replies);
        for asked in &seen.asked {
            if !seen.gone.contains(&asked.conn) {
                assert!(seen.done.contains(asked), "seed {seed}: {asked:?} never ended");
            }
        }
        for conn in conns {
            let replies = world.gone(conn);
            seen.take(replies);
        }
        assert!(world.session.pending.is_empty(), "seed {seed}");
        assert!(world.session.live.is_empty(), "seed {seed}");
        assert_eq!(world.session.views(), 0, "seed {seed}");
        assert!(world.builder.speculated.is_empty(), "seed {seed}: no intent, no speculation");
        assert!(world.builder.promoted.is_empty(), "seed {seed}");
        assert_eq!(world.session.deadline(), None, "seed {seed}");
    }
}

fn lib_target(item: &str) -> Target {
    Target { file: "src/lib.rs".to_owned(), item: item.to_owned() }
}

fn body_edit() -> Vec<Edit> {
    vec![Edit::ReplaceBody { body: "{ 1 }".to_owned(), item: lib_target("a") }]
}

/// What the front end reports for [`body_edit`] on `fn a() {}\n`.
fn body_edited() -> Edited {
    let op = Op::Edit { at: 7, len: 2, path: "src/lib.rs".to_owned(), bytes: b"{ 1 }".to_vec() };
    let item = EditedItem {
        how: How::Body,
        key: Some(ContentId([3; 32])),
        file: "src/lib.rs".to_owned(),
        path: "a".to_owned(),
    };
    Edited { ops: vec![op], items: vec![item] }
}

fn edit_request(base: ContentId) -> Request {
    Request::Edit { base, head: "main".to_owned(), edits: body_edit() }
}

#[test]
fn an_edit_reports_what_it_changed_then_commits_its_ops_once_durable() {
    let mut world = World::new();
    let base = world.commit(&[("src/lib.rs", "fn a() {}\n")]);
    assert_eq!(world.request(1, 1, edit_request(base)), []);
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!(view.map(|it| it.view), Some(View { root: base }));
    assert_eq!(work, Work::Edit(body_edit()));
    assert_eq!(world.session.views(), 1);

    let edited = Response::Edited(body_edited());
    assert_eq!(world.report(job, edited.clone()), [reply(1, 1, edited)]);
    assert_eq!(world.finish(job, true, None), [], "answered once the commit is durable");
    assert_eq!(world.session.views(), 0);
    let root = world.vfs.head("main").unwrap();
    assert_eq!(world.vfs.apply(base, &body_edited().ops).unwrap(), root, "the reported ops");
    assert_eq!(
        world.durable(),
        [reply(1, 1, Response::Committed { root, rebased: false }), done(1, 1, true)]
    );
    let replies = world.request(1, 2, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()), "it is over");
}

#[test]
fn an_edit_that_fails_is_cancelled_or_reports_nothing_commits_nothing() {
    let mut world = World::new();
    let base = world.commit(&[("src/lib.rs", "fn a() {}\n")]);

    world.request(1, 1, edit_request(base));
    let error = ApiError::new("no item `a` in src/lib.rs", Some("src/lib.rs".to_owned()));
    let replies = world.finish(world.last_job(), true, Some(error.clone()));
    assert_eq!(replies, [reply(1, 1, Response::Error(error)), done(1, 1, false)]);

    world.request(1, 2, edit_request(base));
    let job = world.last_job();
    assert_eq!(world.request(1, 3, Request::Cancel { id: 2 }), [done(1, 3, true)]);
    assert_eq!(world.builder.cancelled, [job]);
    world.report(job, Response::Edited(body_edited()));
    assert_eq!(world.finish(job, false, None), [done(1, 2, false)]);

    world.request(1, 4, edit_request(base));
    let job = world.last_job();
    let replies = world.finish(job, true, None);
    assert_eq!(error_at(&replies), Some(format!("job {job}")), "no `Edited` reported");
    assert_eq!(replies.last(), Some(&done(1, 4, false)));

    world.request(2, 1, edit_request(base));
    let job = world.last_job();
    world.report(job, Response::Edited(body_edited()));
    world.gone(2);
    assert_eq!(world.finish(job, true, None), [], "a gone connection's edit is not committed");

    assert_eq!(world.vfs.head("main"), Some(base));
    assert!(world.session.tickets.is_empty());
    assert_eq!(world.session.views(), 0);

    let replies = world.request(1, 5, edit_request(ContentId([7; 32])));
    assert_eq!(error_at(&replies), Some("main".to_owned()), "an unknown base names the head");
}

#[test]
fn an_intent_is_answered_at_once_and_a_request_on_its_root_takes_it_over() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    let filter = vec!["parse".to_owned()];
    let will = Request::WillTest { view: head("main"), filter: filter.clone(), targets: targets(), wait_ms: 100 };
    assert_eq!(
        world.request(1, 1, will),
        [reply(1, 1, Response::Announced { root }), done(1, 1, true)]
    );
    assert!(world.builder.started.is_empty(), "nothing started as a request's");
    let (job, opened, work) = world.builder.speculated[0].clone();
    assert_eq!((opened.view, work), (View { root }, Work::Ahead { targets: targets(), filter }));
    assert_eq!(world.session.views(), 1);
    assert_eq!(world.report(job, diagnostic(Level::Error)), [], "an intent's reports are dropped");

    world.request(2, 1, Request::Test {
        view: head("main"),
        targets: targets(),
        filter: Vec::new(),
        all: false,
        args: Vec::new(),
        ub: None,
    });
    assert_eq!(world.builder.promoted, [job], "another connection's request takes it over");
    world.request(2, 2, Request::Check { view: head("main"), targets: targets() });
    assert_eq!(world.builder.promoted, [job], "once");
    world.tick(u64::MAX);
    world.gone(1);
    assert!(world.builder.cancelled.is_empty(), "a taken-over intent runs to its end");
    assert_eq!(world.finish(job, true, None), []);
    assert_eq!(world.session.views(), 2, "the two requests' views");
}

#[test]
fn a_reach_verify_or_tables_takes_no_intent_over() {
    let mut world = World::new();
    world.commit(&[("src/lib.rs", "")]);
    let will = Request::WillTest { view: head("main"), filter: Vec::new(), targets: targets(), wait_ms: 100 };
    world.request(1, 1, will);
    world.request(1, 2, Request::Verify { view: head("main"), targets: targets() });
    world.request(1, 3, Request::Tables { view: head("main") });
    world.request(1, 4, Request::Reach { from: head("main"), to: head("main"), targets: targets() });
    assert!(world.builder.promoted.is_empty());
}

#[test]
fn a_save_of_the_same_root_takes_it_over_and_one_of_another_cancels_it() {
    let mut world = World::new();
    let base = world.commit(&[("src/lib.rs", "")]);
    let ops = vec![write("src/lib.rs", "fn a() {}")];
    let save = |ops: &Vec<Op>| Request::WillSave {
        ops: ops.clone(),
        head: "main".to_owned(),
        test: true,
        targets: targets(),
        wait_ms: 1000,
    };
    let replies = world.request(1, 1, save(&ops));
    let root = world.vfs.apply(base, &ops).unwrap();
    assert_eq!(replies, [reply(1, 1, Response::Announced { root }), done(1, 1, true)]);
    let (first, opened, work) = world.builder.speculated[0].clone();
    assert_eq!(opened.view, View { root });
    assert_eq!(
        work,
        Work::Test { targets: targets(), filter: Vec::new(), all: false, args: Vec::new(), ub: None }
    );
    assert_eq!(world.vfs.head("main"), Some(base), "an intent moves no head");

    world.request(1, 2, Request::Commit { head: "main".to_owned(), base, ops: ops.clone() });
    assert_eq!(world.builder.promoted, [first]);

    let other = vec![write("src/lib.rs", "fn b() {}")];
    world.request(1, 3, save(&other));
    let second = world.last_speculated();
    assert!(world.builder.cancelled.is_empty(), "the taken-over one is not the newer's to cancel");
    let mut check = Request::WillSave {
        ops: other.clone(),
        head: "elsewhere".to_owned(),
        test: false,
        targets: targets(),
        wait_ms: 1000,
    };
    world.request(2, 1, check.clone());
    let elsewhere = world.last_speculated();
    assert_eq!(world.builder.speculated[2].2, Work::Check(targets()));

    let moved = world.vfs.head("main").unwrap();
    let third = vec![write("src/lib.rs", "fn c() {}")];
    world.request(1, 4, Request::Commit { head: "main".to_owned(), base: moved, ops: third });
    assert_eq!(world.builder.cancelled, [second], "a commit to main of another root");
    assert_eq!(world.builder.promoted, [first], "nothing on another head");

    let now = world.vfs.head("main").unwrap();
    world.call(|session, cx, _| session.head_moved(cx, "elsewhere", now));
    assert_eq!(world.builder.cancelled, [second, elsewhere], "the watcher's move of a head");

    if let Request::WillSave { ops, .. } = &mut check {
        ops.push(Op::Delete { path: "never.rs".to_owned() });
    }
    let replies = world.request(2, 2, check);
    assert_eq!(error_at(&replies), Some("never.rs".to_owned()), "a save whose ops don't apply");
    assert_eq!(world.builder.speculated.len(), 3);

    for job in [first, second, elsewhere] {
        assert_eq!(world.finish(job, false, None), []);
    }
    world.durable();
    assert_eq!(world.session.views(), 0);
}

#[test]
fn an_intent_is_cancelled_by_a_newer_one_its_connections_end_or_its_wait() {
    let mut world = World::new();
    world.commit(&[("src/lib.rs", "")]);
    let will = |wait_ms| Request::WillTest { view: head("main"), filter: Vec::new(), targets: targets(), wait_ms };
    world.tick(1_000);
    world.request(1, 1, will(5));
    let first = world.last_speculated();
    assert_eq!(world.session.deadline(), Some(5_001_000));
    world.request(1, 2, will(5));
    let second = world.last_speculated();
    assert_eq!(world.builder.cancelled, [first], "a newer intent of the connection");
    world.request(2, 1, will(9));
    let third = world.last_speculated();
    assert_eq!(world.gone(2), []);
    assert_eq!(world.builder.cancelled, [first, third], "its connection's end");

    world.tick(5_000_999);
    assert_eq!(world.builder.cancelled, [first, third]);
    world.tick(10);
    assert_eq!(world.builder.cancelled, [first, third], "the clock never goes back");
    world.tick(5_001_000);
    assert_eq!(world.builder.cancelled, [first, third, second], "its wait");
    assert_eq!(world.session.deadline(), None);

    world.request(1, 3, Request::Check { view: head("main"), targets: targets() });
    assert!(world.builder.promoted.is_empty(), "a cancelled intent is not taken over");
    assert_eq!(world.session.views(), 4, "the intents' views stay until they finish");
    for job in [first, second, third] {
        assert_eq!(world.finish(job, false, None), []);
    }
    assert_eq!(world.session.views(), 1);
    world.tick(u64::MAX);
    assert_eq!(world.builder.cancelled.len(), 3, "each cancelled once");
}

#[test]
fn random_intents_are_taken_over_or_cancelled_once_and_free_their_views() {
    for seed in 1..=200u64 {
        let mut rng = SplitMix64::new(seed);
        let mut world = World::new();
        let mut seen = Seen::default();
        let mut conns: Vec<u64> = vec![0, 1, 2];
        let mut next_conn = 3;
        let mut ids: BTreeMap<u64, u64> = BTreeMap::new();
        let mut finished: BTreeSet<JobId> = BTreeSet::new();
        let mut now = 0u64;
        let texts = ["x", "y", "z"];

        for _ in 0..200 {
            let (promoted, cancelled) = (world.builder.promoted.len(), world.builder.cancelled.len());
            match rng.index(8) {
                0..=2 if !conns.is_empty() => {
                    let conn = conns[rng.index(conns.len())];
                    let next = ids.entry(conn).or_insert(0);
                    let id = *next;
                    *next += 1;
                    let text = texts[rng.index(texts.len())];
                    let wait_ms = rng.index(40) as u64;
                    let request = match rng.index(5) {
                        0 => Request::WillSave {
                            ops: vec![write("a.rs", text)],
                            head: "main".to_owned(),
                            test: rng.index(2) == 0,
                            targets: targets(),
                            wait_ms,
                        },
                        1 => Request::WillTest {
                            view: head("main"),
                            filter: Vec::new(),
                            targets: targets(),
                            wait_ms,
                        },
                        2 => {
                            let base = world.vfs.head("main").unwrap_or_else(|| world.vfs.empty());
                            Request::Commit { head: "main".to_owned(), base, ops: vec![write("a.rs", text)] }
                        }
                        _ => Request::Check { view: head("main"), targets: targets() },
                    };
                    seen.asked.insert(to(conn, id));
                    let replies = world.request(conn, id, request);
                    seen.take(replies);
                }
                3 => {
                    now += rng.index(30) as u64 * 1_000_000;
                    world.tick(now);
                    assert!(world.session.deadline().is_none_or(|it| it > now), "seed {seed}");
                }
                4 | 5 => {
                    let mut jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
                    jobs.extend(world.builder.speculated.iter().map(|it| it.0));
                    jobs.retain(|it| !finished.contains(it));
                    if !jobs.is_empty() {
                        let job = jobs[rng.index(jobs.len())];
                        finished.insert(job);
                        let replies = world.finish(job, rng.index(2) == 0, None);
                        seen.take(replies);
                    }
                }
                6 if !conns.is_empty() && rng.index(3) == 0 => {
                    let conn = conns.swap_remove(rng.index(conns.len()));
                    seen.gone.insert(Conn(conn));
                    let replies = world.gone(conn);
                    seen.take(replies);
                    conns.push(next_conn);
                    next_conn += 1;
                }
                _ => {
                    let replies = world.durable();
                    seen.take(replies);
                }
            }
            for job in &world.builder.promoted[promoted..] {
                assert!(!finished.contains(job), "seed {seed}: {job:?} taken over after its end");
            }
            for job in &world.builder.cancelled[cancelled..] {
                assert!(!finished.contains(job), "seed {seed}: {job:?} cancelled after its end");
            }
        }

        let promoted: BTreeSet<JobId> = world.builder.promoted.iter().copied().collect();
        assert_eq!(promoted.len(), world.builder.promoted.len(), "seed {seed}: taken over once");
        let speculated: BTreeSet<JobId> = world.builder.speculated.iter().map(|it| it.0).collect();
        let cancelled: BTreeSet<JobId> =
            world.builder.cancelled.iter().copied().filter(|it| speculated.contains(it)).collect();
        assert_eq!(
            cancelled.len(),
            world.builder.cancelled.iter().filter(|it| speculated.contains(it)).count(),
            "seed {seed}: an intent is cancelled once"
        );
        assert!(promoted.is_subset(&speculated), "seed {seed}: only intents are taken over");
        assert!(promoted.is_disjoint(&cancelled), "seed {seed}: taken over or cancelled, not both");

        let mut jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
        jobs.extend(world.builder.speculated.iter().map(|it| it.0));
        for job in jobs.into_iter().filter(|it| !finished.contains(it)) {
            let replies = world.finish(job, true, None);
            seen.take(replies);
        }
        let replies = world.durable();
        seen.take(replies);
        for asked in &seen.asked {
            if !seen.gone.contains(&asked.conn) {
                assert!(seen.done.contains(asked), "seed {seed}: {asked:?} never ended");
            }
        }
        for conn in conns {
            let replies = world.gone(conn);
            seen.take(replies);
        }
        assert!(world.session.pending.is_empty(), "seed {seed}");
        assert_eq!(world.session.views(), 0, "seed {seed}");
        assert_eq!(world.session.deadline(), None, "seed {seed}");
    }
}
