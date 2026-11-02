use std::collections::{BTreeMap, BTreeSet, VecDeque};

use blaze_api::{ApiError, FactRead, Span};
use blaze_ids::{Fingerprint, rng::SplitMix64};

use super::*;
use crate::config::TargetsConfig;

fn targets() -> TargetsConfig {
    TargetsConfig {
        packages: Vec::new(),
        profile: "dev".to_owned(),
        target: None,
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
    }
}

fn check(name: &str, kind: CheckKind) -> Check {
    Check { name: name.to_owned(), kind, targets: targets(), filter: Vec::new(), all: false }
}

fn settings(one_in: u64) -> Settings {
    Settings {
        default_branch: "main".to_owned(),
        checks: vec![check("blaze/check", CheckKind::Check), check("blaze/test", CheckKind::Test)],
        release_checks: vec![check("blaze/release", CheckKind::Build)],
        oracle_one_in: one_in,
        oracle_targets: targets().to_targets(),
        snapshot_every_secs: 3600,
        snapshot_keep: 2,
    }
}

fn root(commit: &str) -> ContentId {
    ContentId::of("blaze v2 ci test root 1", commit.as_bytes())
}

fn github_id(run: RunId) -> u64 {
    1000 + run.0
}

/// The service, with what GitHub was last sent for each run.
struct Sim {
    service: Service,
    now: u64,
    posted: BTreeMap<RunId, CheckRun>,
    job_of: BTreeMap<RunId, JobId>,
}

impl Sim {
    fn new(one_in: u64) -> Sim {
        let mut out = Vec::new();
        let service = Service::new(settings(one_in), State::default(), &mut out);
        assert!(out.is_empty());
        Sim { service, now: 10_000, posted: BTreeMap::new(), job_of: BTreeMap::new() }
    }

    fn step(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        self.service.step(self.now, input, &mut out);
        for output in &out {
            match output {
                Output::Create { run, job, check } => {
                    self.posted.insert(*run, check.clone());
                    self.job_of.insert(*run, *job);
                }
                Output::Update { run, id, check } => {
                    assert_eq!(*id, github_id(*run));
                    assert_ne!(self.posted.get(run), Some(check), "an update that changes nothing");
                    self.posted.insert(*run, check.clone());
                }
                _ => {}
            }
        }
        out
    }

    /// The event, with GitHub answering each creation at once.
    fn event(&mut self, event: Event) -> Vec<Output> {
        let mut out = self.step(Input::Event(event));
        for (run, _) in creates(&out) {
            out.extend(self.step(Input::Created { run, result: Ok(github_id(run)) }));
        }
        out
    }

    fn status(&self, run: RunId) -> CheckStatus {
        self.posted[&run].status
    }
}

fn creates(out: &[Output]) -> Vec<(RunId, CheckRun)> {
    out.iter()
        .filter_map(|it| match it {
            Output::Create { run, check, .. } => Some((*run, check.clone())),
            _ => None,
        })
        .collect()
}

struct Started {
    job: JobId,
    commit: String,
    fetch: Vec<String>,
    from: Option<Checkout>,
    head: Option<String>,
}

fn checkout(out: &[Output]) -> Option<Started> {
    out.iter().find_map(|it| match it {
        Output::Checkout { job, commit, fetch, from, head } => Some(Started {
            job: *job,
            commit: commit.clone(),
            fetch: fetch.clone(),
            from: from.clone(),
            head: head.clone(),
        }),
        _ => None,
    })
}

fn sends(out: &[Output]) -> Vec<(JobId, RunId, Request)> {
    out.iter()
        .filter_map(|it| match it {
            Output::Send { job, run, request } => Some((*job, *run, request.clone())),
            _ => None,
        })
        .collect()
}

fn error(message: &str, file: &str) -> Response {
    Response::Diagnostic(Diagnostic {
        level: Level::Error,
        code: Some("E0308".to_owned()),
        message: message.to_owned(),
        spans: vec![Span {
            file: file.to_owned(),
            line: 10,
            column: 5,
            end_line: 10,
            end_column: 9,
            label: Some("expected `u32`".to_owned()),
            byte_start: 120,
            byte_end: 124,
            primary: true,
            replacement: None,
            applicability: None,
        }],
        children: Vec::new(),
        facts: vec![FactRead {
            key: blaze_query::QueryKey { kind: blaze_query::QueryKind(3), input: ContentId([4; 32]) },
            what: "the signature of `parser::Token::new`".to_owned(),
        }],
        rendered: String::new(),
        unit: None,
        explanation: None,
    })
}

fn test_result(name: &str, outcome: TestOutcome, output: &str) -> Response {
    Response::TestResult(TestResult {
        binary: "blaze_ci".to_owned(),
        name: name.to_owned(),
        cached: None,
        outcome,
        output: output.as_bytes().to_vec(),
        millis: 12,
        outside: false,
        executable: 0,
    })
}

#[test]
fn a_push_end_to_end() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c1".to_owned() });
    let runs = creates(&out);
    assert_eq!(runs.len(), 2);
    for (_, check) in &runs {
        assert_eq!(check.commit, "c1");
        assert_eq!(check.status, CheckStatus::Queued);
    }
    let started = checkout(&out).unwrap();
    assert_eq!(started.commit, "c1");
    assert_eq!(started.fetch, ["c1"]);
    assert_eq!(started.from, None);
    assert_eq!(started.head.as_deref(), Some("ci/main"));

    let out = sim.step(Input::CheckedOut { job: started.job, result: Ok(root("c1")) });
    assert!(out.contains(&Output::Save), "no save in {out:?}");
    let state = sim.service.state();
    assert_eq!(state.checkouts["branch/main"].checkout, Checkout { commit: "c1".to_owned(), root: root("c1") });
    let sent = sends(&out);
    assert_eq!(sent.len(), 2);
    let view = ViewRef::Root(View { root: root("c1") });
    assert!(matches!(&sent[0].2, Request::Check { view: v, .. } if *v == view));
    assert!(matches!(&sent[1].2, Request::Test { view: v, all: false, .. } if *v == view));
    for (_, run, _) in &sent {
        assert_eq!(sim.status(*run), CheckStatus::InProgress);
    }

    let (job, check_run, _) = sent[0];
    let (_, test_run, _) = sent[1];
    sim.step(Input::Response {
        job,
        run: check_run,
        response: Response::Diagnostic(Diagnostic {
            level: Level::Warning,
            code: Some("unused_variables".to_owned()),
            message: "unused variable".to_owned(),
            spans: Vec::new(),
            children: Vec::new(),
            facts: Vec::new(),
            rendered: String::new(),
            unit: None,
            explanation: None,
        }),
    });
    let out = sim.step(Input::Ended { job, run: check_run, ok: true });
    assert!(!out.iter().any(|it| matches!(it, Output::EndJob { .. })));
    assert_eq!(sim.status(check_run), CheckStatus::Success);
    assert!(sim.posted[&check_run].summary.starts_with("0 errors, 1 warning"));

    sim.step(Input::Response { job, run: test_run, response: test_result("a", TestOutcome::Passed, "") });
    sim.step(Input::Response { job, run: test_run, response: test_result("b", TestOutcome::Unreached, "") });
    let out = sim.step(Input::Ended { job, run: test_run, ok: true });
    assert_eq!(sim.status(test_run), CheckStatus::Success);
    assert!(sim.posted[&test_run].summary.contains("1 passed, 0 failed, 0 ignored, 1 not reached"));
    assert!(out.contains(&Output::EndJob { job }));
    let Some(Output::Snapshot { name }) = out.iter().find(|it| matches!(it, Output::Snapshot { .. }))
    else {
        panic!("a green run of the default branch snapshots: {out:?}")
    };
    assert_eq!(*name, snapshot::name("c1", sim.now));
    assert_eq!(sim.service.jobs(), 0);

    let kept = |at| Kept { id: "snap-1".to_owned(), name: name.clone(), at };
    let out = sim.step(Input::Snapshotted(Ok(kept(sim.now))));
    assert_eq!(out, [Output::Save]);
    assert_eq!(sim.service.state().snapshots, [kept(sim.now)]);

    // Not due again within the interval.
    sim.now += 60;
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c2".to_owned() });
    let started = checkout(&out).unwrap();
    assert_eq!(started.from, Some(Checkout { commit: "c1".to_owned(), root: root("c1") }));
    assert!(out.contains(&Output::Pin { root: root("c1") }));
    let out = sim.step(Input::CheckedOut { job: started.job, result: Ok(root("c2")) });
    // The base held for the checkout, then the old checkout itself, are let go.
    assert_eq!(out.iter().filter(|it| **it == Output::Unpin { root: root("c1") }).count(), 2);
    let mut ended = Vec::new();
    for (job, run, _) in sends(&out) {
        ended.extend(sim.step(Input::Ended { job, run, ok: true }));
    }
    assert!(ended.iter().any(|it| matches!(it, Output::EndJob { .. })));
    assert!(!ended.iter().any(|it| matches!(it, Output::Snapshot { .. })), "the last is a minute old");
}

#[test]
fn a_failed_request_lists_errors_with_their_place_facts_and_test_output() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::Push { branch: "feature".to_owned(), commit: "f1".to_owned() });
    let started = checkout(&out).unwrap();
    assert_eq!(started.head, None);
    let sent = sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("f1")) }));
    let (job, check_run, _) = sent[0];
    let (_, test_run, _) = sent[1];
    sim.step(Input::Response { job, run: check_run, response: error("mismatched types", "src/lib.rs") });
    sim.step(Input::Ended { job, run: check_run, ok: false });
    let summary = &sim.posted[&check_run].summary;
    assert_eq!(sim.status(check_run), CheckStatus::Failure);
    assert!(summary.starts_with("1 error, 0 warnings"), "{summary}");
    assert!(summary.contains("error[E0308] src/lib.rs:10:5: mismatched types"), "{summary}");
    assert!(summary.contains("expected `u32`"), "{summary}");
    assert!(summary.contains("read: the signature of `parser::Token::new`"), "{summary}");

    let output: String = (0..100).map(|n| format!("line {n}\n")).collect();
    sim.step(Input::Response { job, run: test_run, response: test_result("t", TestOutcome::Failed, &output) });
    sim.step(Input::Response {
        job,
        run: test_run,
        response: Response::Error(ApiError::new("the test binary crashed", Some("blaze_ci".to_owned()))),
    });
    let out = sim.step(Input::Ended { job, run: test_run, ok: false });
    let summary = &sim.posted[&test_run].summary;
    assert_eq!(sim.status(test_run), CheckStatus::Failure);
    assert!(summary.contains("0 passed, 1 failed"), "{summary}");
    assert!(summary.contains("test blaze_ci `t` failed"), "{summary}");
    assert!(summary.contains("line 99") && !summary.contains("line 69\n"), "{summary}");
    assert!(summary.contains("the test binary crashed (at blaze_ci)"), "{summary}");
    assert!(out.contains(&Output::EndJob { job }));
    // Not the default branch: no snapshot.
    assert!(!out.iter().any(|it| matches!(it, Output::Snapshot { .. })));
}

#[test]
fn a_pull_request_diffs_from_its_bases_checkout() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c1".to_owned() });
    let started = checkout(&out).unwrap();
    for (job, run, _) in sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("c1")) })) {
        sim.step(Input::Ended { job, run, ok: true });
    }

    let out = sim.event(Event::PullRequest {
        number: 7,
        head: "h1".to_owned(),
        base: "c1".to_owned(),
        from_fork: true,
    });
    for (_, check) in creates(&out) {
        assert_eq!(check.commit, "h1");
    }
    let started = checkout(&out).unwrap();
    assert_eq!(started.fetch, ["refs/pull/7/head", "h1"]);
    assert_eq!(started.from, Some(Checkout { commit: "c1".to_owned(), root: root("c1") }));
    assert_eq!(started.head, None);
    assert!(out.contains(&Output::Pin { root: root("c1") }));
    let out = sim.step(Input::CheckedOut { job: started.job, result: Ok(root("h1")) });
    assert!(out.contains(&Output::Unpin { root: root("c1") }));
    assert_eq!(sim.service.state().checkouts["pr/7"].checkout.root, root("h1"));
    assert_eq!(sim.service.state().checkouts["branch/main"].checkout.root, root("c1"));
    // A fork's requests are the same requests: nothing of the service's is in them.
    for (_, _, request) in sends(&out) {
        assert!(matches!(request, Request::Check { .. } | Request::Test { .. }));
    }
}

#[test]
fn a_pull_request_with_nothing_held_diffs_from_the_empty_tree() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::PullRequest {
        number: 1,
        head: "h".to_owned(),
        base: "b".to_owned(),
        from_fork: false,
    });
    let started = checkout(&out).unwrap();
    assert_eq!(started.from, None);
    assert!(!out.iter().any(|it| matches!(it, Output::Pin { .. })));
}

#[test]
fn a_second_push_to_a_pull_request_cancels_the_first() {
    let mut sim = Sim::new(0);
    let pr = |head: &str| Event::PullRequest {
        number: 7,
        head: head.to_owned(),
        base: "c0".to_owned(),
        from_fork: false,
    };
    let out = sim.event(pr("h1"));
    let first: Vec<RunId> = creates(&out).into_iter().map(|(run, _)| run).collect();
    let started = checkout(&out).unwrap();
    let sent = sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("h1")) }));
    assert_eq!(sent.len(), 2);

    let out = sim.event(pr("h2"));
    assert!(out.contains(&Output::Cancel { job: started.job }));
    for run in &first {
        assert_eq!(sim.status(*run), CheckStatus::Cancelled);
        assert_eq!(sim.posted[run].summary, "Cancelled: replaced by h2.");
        assert_eq!(sim.posted[run].commit, "h1");
    }
    let second = checkout(&out).unwrap();
    assert_eq!(second.commit, "h2");
    assert_eq!(second.from, Some(Checkout { commit: "h1".to_owned(), root: root("h1") }));

    // The cancelled requests end; nothing more is posted for them.
    let mut out = Vec::new();
    for (job, run, _) in &sent {
        out.extend(sim.step(Input::Response { job: *job, run: *run, response: error("late", "a.rs") }));
        out.extend(sim.step(Input::Ended { job: *job, run: *run, ok: false }));
    }
    assert!(!out.iter().any(|it| matches!(it, Output::Update { .. })));
    assert!(out.contains(&Output::EndJob { job: started.job }));
    for run in &first {
        assert_eq!(sim.status(*run), CheckStatus::Cancelled);
    }
}

#[test]
fn a_push_during_a_checkout_waits_for_it_and_diffs_from_it() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c1".to_owned() });
    let first = checkout(&out).unwrap();
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c2".to_owned() });
    assert!(checkout(&out).is_none(), "one checkout of a ref at a time");
    assert!(!out.contains(&Output::Cancel { job: first.job }), "no request is running yet");

    let out = sim.step(Input::CheckedOut { job: first.job, result: Ok(root("c1")) });
    assert!(sends(&out).iter().all(|(job, _, _)| *job != first.job), "the replaced job sends nothing");
    assert!(out.contains(&Output::EndJob { job: first.job }));
    let second = checkout(&out).unwrap();
    assert_eq!(second.commit, "c2");
    assert_eq!(second.from, Some(Checkout { commit: "c1".to_owned(), root: root("c1") }));
    assert_eq!(second.head.as_deref(), Some("ci/main"));
}

#[test]
fn three_tags_the_latest_wins() {
    let mut sim = Sim::new(0);
    let tag = |name: &str| Event::Tag { name: name.to_owned(), commit: format!("commit-{name}") };
    let out = sim.event(tag("v1"));
    let v1 = checkout(&out).unwrap();
    assert_eq!(v1.commit, "commit-v1");

    let out = sim.event(tag("v2"));
    assert!(checkout(&out).is_none());
    let v2: Vec<RunId> = creates(&out).into_iter().map(|(run, _)| run).collect();
    assert_eq!(v2.len(), 1);
    assert_eq!(sim.posted[&v2[0]].summary, "Queued: waiting for release v1 to finish.");

    let out = sim.event(tag("v3"));
    assert!(checkout(&out).is_none());
    let v3: Vec<RunId> = creates(&out).into_iter().map(|(run, _)| run).collect();
    assert_eq!(sim.status(v2[0]), CheckStatus::Cancelled);
    assert_eq!(sim.posted[&v2[0]].summary, "Cancelled: replaced by v3.");

    let out = sim.step(Input::CheckedOut { job: v1.job, result: Ok(root("v1")) });
    let sent = sends(&out);
    assert!(matches!(&sent[..], [(_, _, Request::Build { .. })]));
    assert!(!out.iter().any(|it| matches!(it, Output::Save)), "a tag's root is the job's own");
    let out = sim.step(Input::Ended { job: sent[0].0, run: sent[0].1, ok: true });
    assert!(out.contains(&Output::Unpin { root: root("v1") }));
    assert!(out.iter().any(|it| matches!(it, Output::Snapshot { .. })), "a green release snapshots");
    let v3_started = checkout(&out).unwrap();
    assert_eq!(v3_started.commit, "commit-v3");
    assert_eq!(sim.status(v3[0]), CheckStatus::Queued);

    let sent = sends(&sim.step(Input::CheckedOut { job: v3_started.job, result: Ok(root("v3")) }));
    let out = sim.step(Input::Ended { job: sent[0].0, run: sent[0].1, ok: false });
    assert_eq!(sim.status(v3[0]), CheckStatus::Failure);
    assert!(out.contains(&Output::EndJob { job: v3_started.job }));
    assert_eq!(sim.service.jobs(), 0);
    assert_eq!(sim.service.train, Train::default());
}

#[test]
fn an_oracle_sampled_view_is_verified_and_names_the_fact_that_missed() {
    let mut sim = Sim::new(1);
    let out = sim.event(Event::Push { branch: "feature".to_owned(), commit: "s1".to_owned() });
    let names: Vec<String> = creates(&out).into_iter().map(|(_, check)| check.name).collect();
    assert_eq!(names, ["blaze/check", "blaze/test", oracle::CHECK]);
    let started = checkout(&out).unwrap();
    let sent = sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("s1")) }));
    let Some((job, oracle_run, _)) =
        sent.iter().find(|(_, _, request)| matches!(request, Request::Verify { .. })).cloned()
    else {
        panic!("no verify in {sent:?}")
    };
    let key = |kind| blaze_query::QueryKey { kind: blaze_query::QueryKind(kind), input: ContentId([kind as u8; 32]) };
    let mismatch = Mismatch {
        query: key(5),
        incremental: Fingerprint(1),
        scratch: Fingerprint(2),
        fact: Some(FactRead { key: key(6), what: "the layout of `Token`".to_owned() }),
    };
    sim.step(Input::Response {
        job,
        run: oracle_run,
        response: Response::Verified { mismatches: vec![mismatch] },
    });
    sim.step(Input::Ended { job, run: oracle_run, ok: true });
    assert_eq!(sim.status(oracle_run), CheckStatus::Failure);
    let summary = &sim.posted[&oracle_run].summary;
    assert!(summary.contains("kind 5") && summary.contains("the layout of `Token`"), "{summary}");

    // A clean verification is green; one that ends with no result is not.
    let out = sim.event(Event::Push { branch: "feature".to_owned(), commit: "s2".to_owned() });
    let started = checkout(&out).unwrap();
    let sent = sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("s2")) }));
    for (job, run, request) in &sent {
        if matches!(request, Request::Verify { .. }) {
            sim.step(Input::Response {
                job: *job,
                run: *run,
                response: Response::Verified { mismatches: Vec::new() },
            });
        }
        sim.step(Input::Ended { job: *job, run: *run, ok: true });
        assert_eq!(sim.status(*run), CheckStatus::Success);
    }
    let out = sim.event(Event::Push { branch: "feature".to_owned(), commit: "s3".to_owned() });
    let started = checkout(&out).unwrap();
    let sent = sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("s3")) }));
    let (job, run, _) =
        sent.iter().find(|(_, _, request)| matches!(request, Request::Verify { .. })).cloned().unwrap();
    sim.step(Input::Ended { job, run, ok: true });
    assert_eq!(sim.status(run), CheckStatus::Failure);
}

#[test]
fn a_failed_checkout_posts_failure_naming_it() {
    let mut sim = Sim::new(0);
    let out = sim.event(Event::Push { branch: "main".to_owned(), commit: "c1".to_owned() });
    let runs: Vec<RunId> = creates(&out).into_iter().map(|(run, _)| run).collect();
    let started = checkout(&out).unwrap();
    let what = "git fetch failed: fatal: remote error: upload-pack: not our ref c1".to_owned();
    let out = sim.step(Input::CheckedOut { job: started.job, result: Err(what.clone()) });
    for run in &runs {
        assert_eq!(sim.status(*run), CheckStatus::Failure);
        assert!(sim.posted[run].summary.contains("The checkout of c1 failed"));
        assert!(sim.posted[run].summary.contains(&what));
    }
    assert!(out.contains(&Output::EndJob { job: started.job }));
    assert!(sends(&out).is_empty());
    assert!(!out.iter().any(|it| matches!(it, Output::Snapshot { .. })));
    assert!(sim.service.state().checkouts.is_empty());
}

#[test]
fn updates_before_github_answers_are_posted_once_it_does() {
    let mut sim = Sim::new(0);
    let out = sim.step(Input::Event(Event::Push { branch: "b".to_owned(), commit: "c".to_owned() }));
    let runs = creates(&out);
    let started = checkout(&out).unwrap();
    let out = sim.step(Input::CheckedOut { job: started.job, result: Err("no".to_owned()) });
    assert!(!out.iter().any(|it| matches!(it, Output::Update { .. })));
    assert_eq!(sim.service.jobs(), 0);
    let out = sim.step(Input::Created { run: runs[0].0, result: Ok(github_id(runs[0].0)) });
    assert!(matches!(&out[..], [Output::Update { check, .. }] if check.status == CheckStatus::Failure));
    let out = sim.step(Input::Created { run: runs[1].0, result: Err("502 from GitHub".to_owned()) });
    assert!(matches!(&out[..], [Output::Log(line)] if line.contains("502")));
    assert!(sim.service.runs.is_empty());
}

#[test]
fn a_lost_root_is_dropped_from_the_state() {
    let mut out = Vec::new();
    let mut state = State::default();
    let checkout = Checkout { commit: "c1".to_owned(), root: root("c1") };
    state.checkouts.insert("branch/main".to_owned(), Held { checkout, at: 1 });
    let mut service = Service::new(settings(0), state, &mut out);
    assert_eq!(out, [Output::Pin { root: root("c1") }]);
    out.clear();
    service.step(5, Input::Unpinned { root: root("c1"), why: "unknown root".to_owned() }, &mut out);
    assert!(matches!(&out[0], Output::Log(line) if line.contains("branch/main")));
    assert!(service.state().checkouts.is_empty());
}

#[test]
fn snapshots_beyond_keep_are_removed_and_a_failure_is_logged() {
    let mut out = Vec::new();
    let mut state = State::default();
    for (n, at) in [(1, 100), (2, 200)] {
        state.snapshots.push(Kept { id: format!("s{n}"), name: format!("n{n}"), at });
    }
    let mut service = Service::new(settings(0), state, &mut out);
    service.snapshotting = true;
    let kept = Kept { id: "s3".to_owned(), name: "n3".to_owned(), at: 300 };
    service.step(300, Input::Snapshotted(Ok(kept)), &mut out);
    assert!(out.contains(&Output::RemoveSnapshot { id: "s1".to_owned() }));
    out.clear();
    service.step(301, Input::Removed { id: "s1".to_owned(), result: Ok(()) }, &mut out);
    let ids: Vec<&str> = service.state().snapshots.iter().map(|it| it.id.as_str()).collect();
    assert_eq!(ids, ["s2", "s3"]);
    out.clear();
    service.snapshotting = true;
    service.step(302, Input::Snapshotted(Err("`ix snapshot` failed: quota".to_owned())), &mut out);
    assert!(matches!(&out[..], [Output::Log(line)] if line.contains("quota")));
    assert!(!service.snapshotting);
}

#[test]
fn held_checkouts_are_bounded() {
    let mut sim = Sim::new(0);
    for n in 0..MOST_CHECKOUTS + 3 {
        sim.now += 1;
        let out = sim.event(Event::Push { branch: format!("b{n}"), commit: format!("c{n}") });
        let started = checkout(&out).unwrap();
        for (job, run, _) in sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root(&started.commit)) })) {
            sim.step(Input::Ended { job, run, ok: true });
        }
    }
    assert_eq!(sim.service.state().checkouts.len(), MOST_CHECKOUTS);
    assert!(!sim.service.state().checkouts.contains_key("branch/b0"));
}

/// The rest of the world, answering every output in a random order: GitHub, the workers, the
/// daemon, the pin connection.
struct World {
    rng: SplitMix64,
    /// Inputs waiting, each queue in its order; which queue goes next is random.
    queues: BTreeMap<u64, VecDeque<Input>>,
    next_queue: u64,
    /// The daemon's queue of each run's request.
    of_run: BTreeMap<RunId, u64>,
    /// Commits events named.
    named: BTreeSet<String>,
    pins: BTreeMap<ContentId, i64>,
    ended_jobs: BTreeSet<JobId>,
    awaiting_created: BTreeSet<RunId>,
    /// Runs GitHub never created: nothing about them is posted.
    never_created: BTreeSet<RunId>,
}

impl World {
    fn queue(&mut self, inputs: Vec<Input>) -> u64 {
        let key = self.next_queue;
        self.next_queue += 1;
        self.queues.insert(key, inputs.into());
        key
    }

    fn answer(&mut self, sim: &Sim, out: Vec<Output>) {
        for output in out {
            match output {
                Output::Create { run, check, .. } => {
                    assert!(self.named.contains(&check.commit), "{check:?}");
                    self.awaiting_created.insert(run);
                    let result = if self.rng.index(20) == 0 {
                        Err("GitHub is down".to_owned())
                    } else {
                        Ok(github_id(run))
                    };
                    self.queue(vec![Input::Created { run, result }]);
                }
                Output::Update { check, .. } => assert!(self.named.contains(&check.commit), "{check:?}"),
                Output::Checkout { job, commit, .. } => {
                    assert!(self.named.contains(&commit));
                    let result =
                        if self.rng.index(8) == 0 { Err("git failed".to_owned()) } else { Ok(root(&commit)) };
                    self.queue(vec![Input::CheckedOut { job, result }]);
                }
                Output::Send { job, run, .. } => {
                    let mut inputs = Vec::new();
                    for _ in 0..self.rng.index(3) {
                        let response = match self.rng.index(3) {
                            0 => error("e", "a.rs"),
                            1 => test_result("t", TestOutcome::Failed, "boom"),
                            _ => Response::Verified { mismatches: Vec::new() },
                        };
                        inputs.push(Input::Response { job, run, response });
                    }
                    inputs.push(Input::Ended { job, run, ok: self.rng.index(2) == 0 });
                    let key = self.queue(inputs);
                    self.of_run.insert(run, key);
                }
                Output::Cancel { job } => {
                    let runs: Vec<RunId> =
                        sim.job_of.iter().filter(|(_, it)| **it == job).map(|(run, _)| *run).collect();
                    for run in runs {
                        if let Some(key) = self.of_run.get(&run)
                            && let Some(queue) = self.queues.get_mut(key)
                        {
                            *queue = vec![Input::Ended { job, run, ok: false }].into();
                        }
                    }
                }
                Output::EndJob { job } => {
                    assert!(self.ended_jobs.insert(job), "{job:?} ended twice");
                    for (run, _) in sim.job_of.iter().filter(|(_, it)| **it == job) {
                        let completed = sim.posted[run].status;
                        assert!(
                            self.awaiting_created.contains(run)
                                || self.never_created.contains(run)
                                || matches!(
                                    completed,
                                    CheckStatus::Success | CheckStatus::Failure | CheckStatus::Cancelled
                                ),
                            "{run:?} of ended {job:?} is {completed:?}"
                        );
                    }
                }
                Output::Pin { root } => *self.pins.entry(root).or_default() += 1,
                Output::Unpin { root } => {
                    let count = self.pins.entry(root).or_default();
                    *count -= 1;
                    assert!(*count >= 0, "{root:x} let go more often than held");
                }
                Output::Snapshot { name } => {
                    let kept = Kept { id: name.clone(), name, at: sim.now };
                    self.queue(vec![Input::Snapshotted(Ok(kept))]);
                }
                Output::RemoveSnapshot { id } => {
                    self.queue(vec![Input::Removed { id, result: Ok(()) }]);
                }
                Output::Save | Output::Log(_) => {}
            }
        }
    }

    /// The next waiting input, from a random queue.
    fn next(&mut self) -> Option<Input> {
        if self.queues.is_empty() {
            return None;
        }
        let keys: Vec<u64> = self.queues.keys().copied().collect();
        let key = keys[self.rng.index(keys.len())];
        let queue = self.queues.get_mut(&key).expect("it is there");
        let input = queue.pop_front();
        if queue.is_empty() {
            self.queues.remove(&key);
        }
        input
    }
}

#[test]
fn random_orders_complete_every_check_run_and_post_only_to_named_commits() {
    for seed in 1..=200u64 {
        let mut sim = Sim::new(3);
        let mut world = World {
            rng: SplitMix64::new(seed),
            queues: BTreeMap::new(),
            next_queue: 0,
            of_run: BTreeMap::new(),
            named: BTreeSet::new(),
            pins: BTreeMap::new(),
            ended_jobs: BTreeSet::new(),
            awaiting_created: BTreeSet::new(),
            never_created: BTreeSet::new(),
        };
        let mut events = 0;
        let mut commits = 0;
        loop {
            sim.now += world.rng.index(400) as u64;
            let inject = events < 30 && (world.queues.is_empty() || world.rng.index(3) == 0);
            let input = if inject {
                events += 1;
                commits += 1;
                let commit = format!("{seed}-{commits}");
                world.named.insert(commit.clone());
                let event = match world.rng.index(4) {
                    0 => Event::Push { branch: "main".to_owned(), commit },
                    1 => Event::Push { branch: format!("b{}", world.rng.index(2)), commit },
                    2 => {
                        let base = world.named.iter().next().cloned().unwrap_or_default();
                        Event::PullRequest {
                            number: world.rng.index(3) as u64,
                            head: commit,
                            base,
                            from_fork: world.rng.index(2) == 0,
                        }
                    }
                    _ => Event::Tag { name: format!("v{}", world.rng.index(3)), commit },
                };
                Input::Event(event)
            } else if let Some(input) = world.next() {
                if let Input::Created { run, result } = &input {
                    world.awaiting_created.remove(run);
                    if result.is_err() {
                        world.never_created.insert(*run);
                    }
                }
                if let Input::CheckedOut { result: Ok(root), .. } = &input {
                    *world.pins.entry(*root).or_default() += 1;
                }
                input
            } else {
                break;
            };
            let out = sim.step(input);
            world.answer(&sim, out);
        }
        assert_eq!(sim.service.jobs(), 0, "seed {seed}");
        assert!(sim.service.runs.is_empty(), "seed {seed}");
        for (run, check) in &sim.posted {
            assert!(world.named.contains(&check.commit), "seed {seed}");
            if world.never_created.contains(run) {
                continue;
            }
            assert!(
                matches!(check.status, CheckStatus::Success | CheckStatus::Failure | CheckStatus::Cancelled),
                "seed {seed}: {run:?} left {check:?}"
            );
        }
        for (root, count) in &world.pins {
            let held = sim.service.state().checkouts.values().filter(|it| it.checkout.root == *root).count();
            assert_eq!(*count, held as i64, "seed {seed}: {root:x}");
        }
        assert_eq!(sim.service.train, Train::default(), "seed {seed}");
    }
}

fn tail(output: &[u8]) -> String {
    let mut out = String::new();
    tail_into(&mut out, output);
    out
}

/// The tail as it was found before only the end of the output was read.
fn tail_of_whole(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    if tail.len() > TAIL_BYTES {
        let mut start = tail.len() - TAIL_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail = format!("...{}", &tail[start..]);
    }
    tail.replace("```", "'''")
}

#[test]
fn the_tail_of_a_test_output_is_the_tail_of_the_whole_output() {
    let pieces: [&[u8]; 12] = [
        b"\n",
        b"\r\n",
        b"\n\n\n\n\n\n\n\n",
        b" ",
        b"\t",
        b"word",
        b"```",
        "\u{a0}".as_bytes(),
        "é".as_bytes(),
        b"\xc3",
        b"\xff",
        b"\r",
    ];
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for round in 0..2000 {
        let mut output = Vec::new();
        for _ in 0..rng.index(400) {
            output.extend_from_slice(pieces[rng.index(pieces.len())]);
        }
        if round % 3 == 0 {
            for _ in 0..rng.index(200) {
                output.extend_from_slice(pieces[rng.index(3)]);
            }
        }
        assert_eq!(tail(&output), tail_of_whole(&output), "round {round}: {output:?}");
    }
    let long: Vec<u8> = (0..10_000).flat_map(|line| format!("line {line} of a long output\n").into_bytes()).collect();
    assert_eq!(tail(&long), tail_of_whole(&long));
    assert_eq!(tail(b""), tail_of_whole(b""));
    let wide: Vec<u8> = "é```".repeat(3000).into_bytes();
    assert_eq!(tail(&wide), tail_of_whole(&wide));
    let mut after = "kept ```".to_owned();
    tail_into(&mut after, b"a```b\n");
    assert_eq!(after, "kept ```a'''b", "only the tail is escaped, after what was there");
}

/// What a request's responses said, as the report kept them before its lists were one string
/// each: a string per item, joined at the summary.
#[derive(Default)]
struct OldReport {
    errors: usize,
    warnings: usize,
    listed: Vec<String>,
    passed: usize,
    failed: usize,
    ignored: usize,
    unreached: usize,
    failures: Vec<String>,
    artifacts: usize,
    said: Vec<String>,
    unexpected: Vec<String>,
}

impl OldReport {
    fn add(&mut self, response: &Response) {
        match response {
            Response::Diagnostic(diagnostic) => match diagnostic.level {
                Level::Error => {
                    self.errors += 1;
                    if self.listed.len() < MOST_LISTED {
                        let mut text = String::new();
                        render_diagnostic_into(&mut text, diagnostic).unwrap();
                        self.listed.push(text);
                    }
                }
                Level::Warning => self.warnings += 1,
                Level::Note | Level::Help | Level::FailureNote => {}
            },
            Response::TestResult(test) => match test.outcome {
                TestOutcome::Passed => self.passed += 1,
                TestOutcome::Ignored => self.ignored += 1,
                TestOutcome::Unreached => self.unreached += 1,
                TestOutcome::Failed => {
                    self.failed += 1;
                    if self.failures.len() < MOST_LISTED {
                        self.failures.push(format!(
                            "test {} `{}` failed after {} ms; the end of its output:\n```\n{}\n```",
                            test.binary,
                            test.name,
                            test.millis,
                            tail_of_whole(&test.output)
                        ));
                    }
                }
            },
            Response::Built { artifacts, .. } => self.artifacts += artifacts.len(),
            Response::Error(error) => self.said.push(match &error.at {
                Some(at) => format!("{} (at {at})", error.what),
                None => error.what.clone(),
            }),
            Response::Done { .. } | Response::Snapshotted { .. } => {
                if self.unexpected.len() < MOST_LISTED {
                    let mut shown = format!("{response:?}");
                    if shown.len() > 200 {
                        let mut end = 200;
                        while !shown.is_char_boundary(end) {
                            end -= 1;
                        }
                        shown.truncate(end);
                        shown.push_str("...");
                    }
                    self.unexpected.push(shown);
                }
            }
            _ => unreachable!("not made by the test"),
        }
    }

    fn summary(&self, kind: CheckKind, ok: bool) -> String {
        let mut text = format!(
            "{} error{}, {} warning{}",
            self.errors,
            plural(self.errors),
            self.warnings,
            plural(self.warnings)
        );
        match kind {
            CheckKind::Test => text += &format!(
                "; tests: {} passed, {} failed, {} ignored, {} not reached by the change (their \
                 last results stand)",
                self.passed, self.failed, self.ignored, self.unreached
            ),
            CheckKind::Build => text += &format!("; {} artifacts built", self.artifacts),
            CheckKind::Check => {}
        }
        text.push('.');
        if !self.listed.is_empty() {
            text.push_str("\n\n## Errors\n");
            for error in &self.listed {
                text += &format!("\n{error}\n");
            }
            if self.errors > self.listed.len() {
                text += &format!("\nand {} more errors\n", self.errors - self.listed.len());
            }
        }
        if !self.failures.is_empty() {
            text.push_str("\n\n## Failed tests\n");
            for failure in &self.failures {
                text += &format!("\n{failure}\n");
            }
            if self.failed > self.failures.len() {
                text += &format!("\nand {} more failed tests\n", self.failed - self.failures.len());
            }
        }
        if !self.said.is_empty() {
            text.push_str("\n\n## The daemon's errors\n");
            for error in &self.said {
                text += &format!("\n- {error}");
            }
            text.push('\n');
        }
        if !self.unexpected.is_empty() {
            text.push_str("\n\n## Responses this request should not have had\n");
            for response in &self.unexpected {
                text += &format!("\n- {response}");
            }
            text.push('\n');
        }
        if !ok && self.errors == 0 && self.failed == 0 && self.said.is_empty() {
            text.push_str("\n\nThe daemon ended the request as failed and said nothing more.");
        }
        text
    }
}

#[test]
fn a_report_in_one_string_a_list_summarizes_as_one_string_an_item_did() {
    let mut rng = SplitMix64::new(0x5eed_0f_5e1f);
    let outputs = ["", "ok\n", "a```b\nline two\r\n\n", "é\u{ff}\n", "panicked at src/lib.rs:3:5\n"];
    for round in 0..300 {
        let (mut new, mut old) = (Report::default(), OldReport::default());
        let count = if round % 10 == 0 { 2 * MOST_LISTED + 20 } else { rng.index(30) };
        for at in 0..count {
            let response = match rng.index(9) {
                0 | 1 => error(&format!("mismatched types {at}"), "src/lib.rs"),
                2 => Response::Diagnostic(Diagnostic {
                    level: [Level::Warning, Level::Note, Level::Help][rng.index(3)],
                    code: None,
                    message: "unused".to_owned(),
                    spans: Vec::new(),
                    children: Vec::new(),
                    facts: Vec::new(),
                    rendered: String::new(),
                    unit: None,
                    explanation: None,
                }),
                3 | 4 => {
                    let outcome = [TestOutcome::Passed, TestOutcome::Failed, TestOutcome::Ignored, TestOutcome::Unreached]
                        [rng.index(4)];
                    let output = outputs[rng.index(outputs.len())].repeat(1 + rng.index(40));
                    Response::TestResult(TestResult {
                        binary: "blaze_ci".to_owned(),
                        name: format!("t{at}"),
                        cached: None,
                        outcome,
                        output: output.into_bytes(),
                        millis: at as u64,
                        outside: false,
                        executable: 0,
                    })
                }
                5 => Response::Error(ApiError::new(format!("lost {at}"), (at % 2 == 0).then(|| "a.sock".to_owned()))),
                6 => Response::Done { ok: at % 2 == 0 },
                7 => Response::Snapshotted { heads: vec![("é".repeat(150), ContentId([5; 32]))] },
                _ => Response::Built { members: 1, profile: "dev".to_owned(), artifacts: Vec::new() },
            };
            old.add(&response);
            new.add(response);
        }
        let ok = rng.index(2) == 0;
        for kind in [CheckKind::Check, CheckKind::Build, CheckKind::Test] {
            assert_eq!(new.summary(kind, ok), old.summary(kind, ok), "round {round} {kind:?}");
        }
    }
}

/// A pull request's job with its checkout done and its two requests sent, each run with its id
/// from GitHub.
fn running_pull(sim: &mut Sim) -> Vec<(JobId, RunId, Request)> {
    let event = Event::PullRequest { number: 7, head: "h1".repeat(20), base: "b1".repeat(20), from_fork: false };
    let started = checkout(&sim.event(event)).unwrap();
    sends(&sim.step(Input::CheckedOut { job: started.job, result: Ok(root("h1")) }))
}

#[test]
fn a_warm_service_step_allocates_only_what_it_posts() {
    let mut sim = Sim::new(0);
    let sent = running_pull(&mut sim);
    let ((job, check_run, _), (_, test_run, _)) = (sent[0].clone(), sent[1].clone());
    let now = sim.now;
    let mut out = Vec::with_capacity(64);
    let mut step = |service: &mut Service, input: Input, out: &mut Vec<Output>| {
        out.clear();
        service.step(now, input, out);
    };

    for (what, response) in [
        ("a passed test", test_result("a", TestOutcome::Passed, "")),
        ("an ignored test", test_result("b", TestOutcome::Ignored, "")),
        ("a test the change did not reach", test_result("c", TestOutcome::Unreached, "")),
    ] {
        let input = Input::Response { job, run: test_run, response };
        blaze_alloc_count::assert_at_most(0, what, || step(&mut sim.service, input, &mut out));
    }
    let warning = Response::Diagnostic(Diagnostic {
        level: Level::Warning,
        code: None,
        message: "unused".to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: String::new(),
        unit: None,
        explanation: None,
    });
    let input = Input::Response { job, run: check_run, response: warning };
    blaze_alloc_count::assert_at_most(0, "a warning", || step(&mut sim.service, input, &mut out));
    for at in 0..MOST_LISTED + 50 {
        let input = Input::Response { job, run: check_run, response: error(&format!("e{at}"), "a.rs") };
        let budget = u64::from(at < MOST_LISTED);
        blaze_alloc_count::assert_at_most(
            budget,
            "an error: listed, at most its list's buffer growing; past the most listed, counted",
            || step(&mut sim.service, input, &mut out),
        );
    }
    assert!(out.is_empty(), "a response posts nothing: {out:?}");

    let ended = Input::Ended { job, run: test_run, ok: true };
    blaze_alloc_count::assert_at_most(
        4,
        "a green run's end: its summary, and the check run handed to the GitHub thread (its name, \
         commit and summary)",
        || step(&mut sim.service, ended, &mut out),
    );
    assert!(matches!(&out[..], [Output::Update { check, .. }] if check.status == CheckStatus::Success), "{out:?}");

    let ignored = Input::Ended { job, run: test_run, ok: true };
    blaze_alloc_count::assert_at_most(0, "an end already ended", || step(&mut sim.service, ignored, &mut out));
    let late = Input::Created { run: test_run, result: Ok(github_id(test_run)) };
    blaze_alloc_count::assert_at_most(0, "a creation already answered", || step(&mut sim.service, late, &mut out));
    assert!(out.is_empty(), "{out:?}");
}
