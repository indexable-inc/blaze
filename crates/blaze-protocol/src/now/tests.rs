//! The fold on small graphs (a chain, a diamond, Andrew's build of 2026-11-05, a program that is
//! no build), its phase and memory, `Now`'s CBOR, and a property over random graphs: every
//! unfinished task off the frontier is held by a blocker.

use super::*;
use crate::Message;
use crate::cbor::{check, from_slice};

const S: u64 = 1_000_000_000;
const GIB: u64 = 1 << 30;

fn node(id: u32, kind: NodeKind, name: &str, deps: &[u32]) -> Event {
    Event::Node { id, kind, name: name.into(), deps: deps.to_vec() }
}

/// One [`Event::Nodes`] of `nodes`, the first `first`.
fn batch(first: u32, nodes: &[(NodeKind, String, Vec<u32>)]) -> Event {
    let (mut names, mut name_at, mut deps, mut deps_at) = (String::new(), vec![0], vec![], vec![0]);
    for (_, name, mine) in nodes {
        names.push_str(name);
        name_at.push(names.len() as u32);
        deps.extend_from_slice(mine);
        deps_at.push(deps.len() as u32);
    }
    let kinds = nodes.iter().map(|it| it.0).collect();
    Event::Nodes { first, kinds, names, name_at, deps, deps_at }
}

fn moved(fold: &mut Fold, id: u32, state: NodeState) {
    fold.take_event(&Event::Moved { id, state });
}

fn span(name: &str, for_ms: u64) -> Span {
    Span { name: name.into(), item: None, krate: None, awaits: None, for_ms }
}

fn look<'a>(threads: &'a [Worker], downloads: &'a [Download], memory: &'a Memory) -> Look<'a> {
    Look { workers: 16, threads, downloads, stage: "", stall: None, memory, expanders: &[], expansion_threads: 0 }
}

fn named(now: &Now) -> Vec<&str> {
    now.blockers.iter().map(|it| it.task.name.as_str()).collect()
}

/// `c` waits on `b`, `b` on running `a`: one blocker, holding both, a chain of two. The ids go
/// against the deps, so the fold may not assume a dep has the smaller id.
#[test]
fn a_chain_waits_on_its_running_head() {
    let mut fold = Fold::new();
    fold.advance_clock(0);
    fold.take_event(&node(0, NodeKind::Link, "c", &[1]));
    fold.take_event(&node(1, NodeKind::Bodies, "b", &[2]));
    fold.take_event(&node(2, NodeKind::Bodies, "a", &[]));
    moved(&mut fold, 2, NodeState::Ready);
    moved(&mut fold, 2, NodeState::Running);
    fold.advance_clock(3 * S);
    let memory = Memory::default();
    let now = fold.look(&look(&[], &[], &memory));
    assert_eq!(named(&now), ["a"], "{now:?}");
    let a = &now.blockers[0];
    assert_eq!((a.running, a.holds, a.chain, a.for_ms), (true, 2, 2, 3000));
    assert_eq!(a.behind, ["b"]);
    assert_eq!(now.tally, Tally { waiting: 2, ready: 0, running: 1, done: 0, failed: 0, total: 3 });
    assert_eq!(now.phase, Phase::Compiling);
    assert_eq!(now.quiet_ms, 3000, "nothing settled since the clock began");
}

/// `top` waits on `left` and `right` (on `right` twice), both on `base`; `lone` waits on
/// nothing. While `base` runs it holds the three; once it is built, `left` and `right` each
/// hold `top`, the one ready longer first, and `lone`, holding nothing, comes last.
#[test]
fn a_diamond_is_held_by_its_base_and_then_by_both_sides() {
    let mut fold = Fold::new();
    fold.advance_clock(0);
    fold.take_event(&node(0, NodeKind::Link, "top", &[1, 2, 2]));
    fold.take_event(&node(1, NodeKind::Bodies, "left", &[3]));
    fold.take_event(&node(2, NodeKind::Bodies, "right", &[3]));
    fold.take_event(&node(3, NodeKind::Names, "base", &[]));
    fold.take_event(&node(4, NodeKind::Script, "lone", &[]));
    moved(&mut fold, 3, NodeState::Ready);
    fold.advance_clock(S);
    moved(&mut fold, 3, NodeState::Running);
    fold.advance_clock(5 * S);
    let memory = Memory::default();
    let now = fold.look(&look(&[], &[], &memory));
    assert_eq!(named(&now), ["base", "lone"], "{now:?}");
    let base = &now.blockers[0];
    assert_eq!((base.holds, base.chain, base.for_ms), (3, 2, 4000));
    assert_eq!(base.behind, ["left", "right"]);
    let lone = &now.blockers[1];
    assert_eq!((lone.running, lone.holds, lone.chain, lone.for_ms), (false, 0, 0, 5000));

    fold.advance_clock(6 * S);
    moved(&mut fold, 3, NodeState::Built);
    moved(&mut fold, 2, NodeState::Ready);
    fold.advance_clock(7 * S);
    moved(&mut fold, 1, NodeState::Ready);
    moved(&mut fold, 1, NodeState::Running);
    fold.advance_clock(10 * S);
    let now = fold.look(&look(&[], &[], &memory));
    assert_eq!(named(&now), ["right", "left", "lone"], "{now:?}");
    let [right, left, _] = &now.blockers[..] else { unreachable!() };
    assert_eq!((right.running, right.holds, right.chain, right.for_ms), (false, 1, 1, 4000));
    assert_eq!((left.running, left.holds, left.chain, left.for_ms), (true, 1, 1, 3000));
    assert_eq!(right.behind, ["top"]);
    assert_eq!(left.behind, ["top"]);
    assert_eq!(now.tally, Tally { waiting: 2, ready: 1, running: 1, done: 1, failed: 0, total: 5 });
    assert_eq!(now.quiet_ms, 4000, "since base settled");
    assert_eq!(now.slowest.len(), 1);
    assert_eq!((now.slowest[0].task.name.as_str(), now.slowest[0].ms), ("base", 5000));
}

/// Andrew's build of 2026-11-05: `codegen core` runs for 4.5 minutes in `copy_prop` asking
/// `is_freeze`, with 82 tasks (build scripts among them) behind it in two chains of 41, on 16
/// workers of which 15 are idle, and the daemon at 99 GiB against 24.
#[test]
fn one_codegen_holds_back_the_build() {
    let core = 10;
    let mut nodes: Vec<(NodeKind, String, Vec<u32>)> =
        (0..core).map(|id| (NodeKind::Bodies, format!("bodies dep{id}"), vec![])).collect();
    nodes.push((NodeKind::Codegen, "codegen core".into(), (0..core).collect()));
    for (first, krate) in [(11, "std"), (52, "libc")] {
        for id in first..first + 41 {
            let (kind, name) = match id - first {
                0 => (NodeKind::Script, format!("{krate} build script")),
                1 => (NodeKind::ScriptRun, format!("{krate} script-run")),
                at if at % 2 == 0 => (NodeKind::Bodies, format!("bodies {krate}{at}")),
                at => (NodeKind::Codegen, format!("codegen {krate}{at}")),
            };
            let dep = if id == first { core } else { id - 1 };
            nodes.push((kind, name, vec![dep]));
        }
    }
    assert_eq!(nodes.len(), 93);

    let mut fold = Fold::new();
    fold.advance_clock(0);
    fold.take_event(&batch(0, &nodes));
    for id in 0..core {
        moved(&mut fold, id, NodeState::Ready);
        moved(&mut fold, id, NodeState::Running);
    }
    fold.advance_clock(S);
    for id in 0..core {
        moved(&mut fold, id, NodeState::Built);
    }
    moved(&mut fold, core, NodeState::Ready);
    moved(&mut fold, core, NodeState::Running);
    fold.advance_clock(271 * S);

    let mut threads: Vec<Worker> = (0..16)
        .map(|at| Worker { thread: format!("blaze-worker-{at}"), task: None, open: vec![] })
        .collect();
    let freeze = Span {
        name: "is_freeze".into(),
        item: Some("core::ptr::is_freeze".into()),
        krate: Some("core".into()),
        awaits: None,
        for_ms: 250_000,
    };
    threads[3].task = Some(u64::from(core));
    threads[3].open = vec![
        span("task", 270_000),
        span("codegen", 269_000),
        span("copy_prop", 260_000),
        freeze.clone(),
    ];
    let memory = Memory {
        held: 99 * GIB,
        limit: 24 * GIB,
        over: Some("mir".into()),
        releases: 5,
        ..Memory::default()
    };
    let now = fold.look(&look(&threads, &[], &memory));

    assert_eq!((now.busy.len(), now.idle()), (1, 15));
    let busy = &now.busy[0];
    assert_eq!(busy.thread, "blaze-worker-3");
    let task = Task {
        id: u64::from(core),
        kind: NodeKind::Codegen.name().into(),
        name: "codegen core".into(),
    };
    assert_eq!(busy.task.as_ref(), Some(&task));
    assert_eq!(busy.span, freeze);
    assert_eq!(busy.path, ["codegen", "copy_prop"]);

    assert_eq!(now.blockers.len(), 1, "{:?}", now.blockers);
    let blocker = &now.blockers[0];
    assert_eq!(blocker.task, task);
    assert_eq!((blocker.running, blocker.holds, blocker.chain), (true, 82, 41));
    assert_eq!(blocker.for_ms, 270_000);
    assert_eq!(blocker.span.as_ref(), Some(&freeze));
    assert_eq!(blocker.behind, ["std build script", "libc build script"]);

    let tally = Tally { waiting: 82, ready: 0, running: 1, done: 10, failed: 0, total: 93 };
    assert_eq!(now.tally, tally);
    assert_eq!(now.phase, Phase::Compiling);
    assert_eq!(now.quiet_ms, 270_000);
    assert!(now.slow() && !now.stalled(), "a worker is busy: slow, not stalled");
    assert_eq!(now.slowest.len(), SLOWEST);
    assert_eq!((now.slowest[0].task.id, now.slowest[0].ms), (0, 1000), "ties go by id");

    assert_eq!(now.memory.pressure(), Pressure::Over);
    assert_eq!((now.memory.peak, now.memory.releases), (99 * GIB, 0), "counted from the first look");
    assert_eq!(now.running.len(), 1);
    assert_eq!((now.running[0].task.id, now.running[0].for_ms), (u64::from(core), 270_000));
    assert_eq!(now.running[0].thread.as_deref(), Some("blaze-worker-3"));
    assert_eq!(now.running[0].span.as_ref(), Some(&freeze));
    assert_eq!(now.recent, 0, "the settles were 270 s ago");
    assert_eq!(now.memory.over.as_deref(), Some("mir"));

    // Memory let go by two releases: the peak stays, the releases count since the first look.
    let lower = Memory { held: 20 * GIB, limit: 24 * GIB, releases: 7, ..Memory::default() };
    let now = fold.look(&look(&threads, &[], &lower));
    assert_eq!((now.memory.held, now.memory.peak), (20 * GIB, 99 * GIB));
    assert_eq!(now.memory.releases, 2);
    assert_eq!(now.memory.pressure(), Pressure::Rising);
    assert!(now.slow());

    // No worker busy, no process running and nothing settled for 290 s: stalled.
    fold.advance_clock(291 * S);
    let idle: Vec<Worker> = (0..16)
        .map(|at| Worker { thread: format!("blaze-worker-{at}"), task: None, open: vec![] })
        .collect();
    let now = fold.look(&look(&idle, &[], &lower));
    assert_eq!((now.busy.len(), now.quiet_ms), (0, 290_000));
    assert!(now.stalled() && !now.slow());
    assert_eq!(now.blockers[0].span, None);
}

/// A download in flight with no task running is fetching, before the graph has a node and
/// after; then the phase follows the stage and the tasks.
#[test]
fn a_download_with_nothing_running_is_fetching() {
    let mut fold = Fold::new();
    fold.advance_clock(0);
    let memory = Memory::default();
    let llvm = Download {
        what: "llvm-21.1.8-aarch64-apple-darwin.tar.xz".into(),
        done: 34_000_000,
        total: Some(410_000_000),
        rate: Some(260_000),
        idle_ms: 200,
        elapsed_ms: 130_000,
    };
    let downloads = std::slice::from_ref(&llvm);
    assert_eq!(fold.look(&look(&[], &[], &memory)).phase, Phase::Starting);
    fold.advance_clock(30 * S);
    let now = fold.look(&look(&[], downloads, &memory));
    assert_eq!(now.phase, Phase::Fetching);
    assert_eq!(now.quiet_ms, 200, "a byte came 200 ms ago");
    assert_eq!(now.downloads, [llvm.clone()]);

    fold.take_event(&node(0, NodeKind::Manifest, "manifest", &[]));
    fold.take_event(&node(1, NodeKind::Std, "std", &[0]));
    assert_eq!(fold.look(&look(&[], downloads, &memory)).phase, Phase::Fetching);
    assert_eq!(fold.look(&look(&[], &[], &memory)).phase, Phase::Resolving);
    let planning = Look { stage: "waits for its plan", ..look(&[], &[], &memory) };
    assert_eq!(fold.look(&planning).phase, Phase::Planning);

    moved(&mut fold, 0, NodeState::Built);
    moved(&mut fold, 1, NodeState::Ready);
    assert_eq!(fold.look(&look(&[], &[], &memory)).phase, Phase::Compiling);
    moved(&mut fold, 1, NodeState::Running);
    assert_eq!(fold.look(&look(&[], downloads, &memory)).phase, Phase::Compiling);
    moved(&mut fold, 1, NodeState::Cached { remote: true });
    assert_eq!(fold.look(&look(&[], downloads, &memory)).phase, Phase::Finishing);
}

/// A program that is no build, said as task records: ids past `u32::MAX`, kinds of its own
/// layers, a dep said after the task that waits on it. Its phase is running, the whole time,
/// and its blockers, tally and slowest are a build's.
#[test]
fn a_program_that_is_no_build_is_running_with_wide_ids() {
    let big = u64::from(u32::MAX) + 1;
    let (crawl, fetch, parse, index) = (big, big + 7, 3 * big, u64::MAX);
    let record = |id: u64, kind: &str, name: &str, deps: &[u64]| TaskRecord {
        id,
        kind: kind.into(),
        name: name.into(),
        deps: deps.to_vec(),
    };
    let mut fold = Fold::new();
    fold.advance_clock(0);
    fold.take(&record(index, "index", "index all", &[parse]));
    fold.take(&record(parse, "parse", "parse pages", &[fetch]));
    fold.take(&record(fetch, "fetch-page", "fetch pages", &[crawl]));
    fold.take(&record(crawl, "crawl", "crawl site", &[]));
    let memory = Memory::default();
    let now = fold.look(&look(&[], &[], &memory));
    assert_eq!(now.phase, Phase::Running, "{now:?}");
    assert_eq!(named(&now), ["crawl site"]);

    fold.moved(crawl, State::Ready);
    fold.moved(crawl, State::Running);
    fold.advance_clock(2 * S);
    let threads = [Worker {
        thread: "crawler-0".into(),
        task: Some(crawl),
        open: vec![span("task", 2000), span("get", 1500)],
    }];
    let now = fold.look(&look(&threads, &[], &memory));
    assert_eq!(now.phase, Phase::Running, "not compiling: no task is a build's");
    let crawling = Task { id: crawl, kind: "crawl".into(), name: "crawl site".into() };
    assert_eq!(now.busy[0].task.as_ref(), Some(&crawling));
    let blocker = &now.blockers[0];
    assert_eq!(blocker.task, crawling);
    assert_eq!((blocker.running, blocker.holds, blocker.chain, blocker.for_ms), (true, 3, 3, 2000));
    assert_eq!(blocker.behind, ["fetch pages"]);
    assert_eq!(blocker.span.as_ref().map(|it| it.name.as_str()), Some("get"));

    fold.moved(crawl, State::Done);
    fold.moved(fetch, State::Ready);
    fold.moved(fetch, State::Running);
    fold.advance_clock(3 * S);
    fold.moved(fetch, State::Failed);
    fold.moved(parse, State::Cancelled);
    fold.moved(index, State::Cancelled);
    fold.moved(big + 1, State::Done);
    let now = fold.look(&look(&[], &[], &memory));
    assert_eq!(now.phase, Phase::Running);
    assert!(now.blockers.is_empty(), "{now:?}");
    assert_eq!(now.tally, Tally { waiting: 0, ready: 0, running: 0, done: 1, failed: 3, total: 4 });
    let slowest: Vec<(u64, u64)> = now.slowest.iter().map(|it| (it.task.id, it.ms)).collect();
    assert_eq!(slowest, [(crawl, 2000), (fetch, 1000)]);

    // One task of a build's kind and it is a build again.
    fold.take(&record(big + 1, NodeKind::Link.name(), "link", &[]));
    fold.moved(big + 1, State::Running);
    assert_eq!(fold.look(&look(&[], &[], &memory)).phase, Phase::Linking);
}

/// Each pressure line, on a limit of 1000: three quarters is 750, the release line 875.
#[test]
fn memory_pressure_at_each_line() {
    let at = |held: u64, limit: u64, over: Option<&str>, held_back: u32| {
        let over = over.map(str::to_owned);
        Memory { held, peak: held, limit, over, held_back, releases: 0 }.pressure()
    };
    assert_eq!(at(99 * GIB, 0, None, 0), Pressure::Calm, "no limit");
    assert_eq!(at(0, 0, None, 3), Pressure::Squeezed, "no limit, tasks held back");
    let limit = 1000;
    assert_eq!(at(700, limit, None, 0), Pressure::Calm, "70%");
    assert_eq!(at(749, limit, None, 0), Pressure::Calm);
    assert_eq!(at(750, limit, None, 0), Pressure::Rising);
    assert_eq!(at(800, limit, None, 0), Pressure::Rising, "80%");
    assert_eq!(at(874, limit, None, 0), Pressure::Rising);
    assert_eq!(at(875, limit, None, 0), Pressure::Squeezed);
    assert_eq!(at(900, limit, None, 0), Pressure::Squeezed, "90%");
    assert_eq!(at(100, limit, Some("mir"), 0), Pressure::Squeezed, "a subsystem over its share");
    assert_eq!(at(100, limit, None, 2), Pressure::Squeezed, "tasks held back");
    assert_eq!(at(999, limit, None, 0), Pressure::Squeezed);
    assert_eq!(at(1000, limit, None, 0), Pressure::Over, "100%");
    assert_eq!(at(2000, limit, None, 0), Pressure::Over);
}

/// A filled `Now` is written as it comes, no map of it put in order (so every `keys [..]` list
/// of this module is in its encoding's order), to the bytes the reference encoder gives, and
/// reads back equal; the same entries as JSON has.
#[test]
fn a_now_is_written_in_its_keys_order_and_round_trips() {
    let task = Task { id: 10, kind: "codegen".into(), name: "codegen core".into() };
    let span = Span {
        name: "is_freeze".into(),
        item: Some("core::ptr::is_freeze".into()),
        krate: Some("core".into()),
        awaits: Some("a lock".into()),
        for_ms: 270_000,
    };
    let now = Now {
        at_ms: 914_000,
        phase: Phase::Testing,
        stage: "waits for its pinned native inputs, 1 of 1 layouts left".into(),
        stall: Some(
            "convoy: 61 behind thread 7 on lock flight.intern (oldest 2400 ms); the head is running"
                .into(),
        ),
        tally: Tally { waiting: 81, ready: 2, running: 1, done: 488, failed: 3, total: 575 },
        workers: 16,
        busy: vec![Busy {
            thread: "blaze-worker-3".into(),
            task: Some(task.clone()),
            span: span.clone(),
            path: vec!["codegen".into(), "copy_prop".into()],
        }],
        blockers: vec![Blocker {
            task: task.clone(),
            running: true,
            for_ms: 270_000,
            span: Some(span),
            holds: 82,
            chain: 41,
            behind: vec!["std build script".into(), "libc build script".into()],
        }],
        downloads: vec![Download {
            what: "llvm-21.1.8-aarch64-apple-darwin.tar.xz".into(),
            done: 34_000_000,
            total: Some(410_000_000),
            rate: Some(260_000),
            idle_ms: 200,
            elapsed_ms: 130_000,
        }],
        slowest: vec![Took { task, ms: 72_000 }],
        quiet_ms: 270_000,
        memory: Memory {
            held: 99 * GIB,
            peak: 101 * GIB,
            limit: 24 * GIB,
            over: Some("mir".into()),
            held_back: 4,
            releases: 7,
        },
    };
    let message = Message::new(3, &now);
    let mut writer = crate::cbor::Writer::new();
    let mut out = Vec::new();
    writer.append(&mut out, &message).unwrap();
    assert_eq!(writer.permuted(), 0, "a keys list is out of its encoding's order");
    assert_eq!(out, crate::cbor::reference::to_vec(&message).unwrap());
    check(&out).unwrap();
    let read: Message<Now> = from_slice(&out).unwrap();
    assert_eq!(read.body, now);
    let entries: serde_json::Value = from_slice(&out).unwrap();
    assert_eq!(entries, serde_json::to_value(&message).unwrap());
}

/// Random graphs (deps only on smaller ids, a task settled or under way only once its deps
/// settled), said in one batch: under the cap, the blockers are the frontier, ranked, each
/// holding exactly the unfinished tasks off the frontier that wait on it with the longest
/// chain behind it, and every such task is held by one.
#[hegel::test]
fn every_task_off_the_frontier_is_held_by_a_blocker(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let len: usize = tc.draw(gs::integers::<usize>().max_value(24));
    let (mut deps, mut states): (Vec<Vec<u32>>, Vec<NodeState>) = (vec![], vec![]);
    for id in 0..len {
        let mut mine = vec![];
        if id > 0 {
            for _ in 0..tc.draw(gs::integers::<usize>().max_value(3)) {
                mine.push(tc.draw(gs::integers::<u32>().max_value(id as u32 - 1)));
            }
        }
        let state = if mine.iter().any(|&dep| !states[dep as usize].settled()) {
            NodeState::Waiting
        } else {
            match tc.draw(gs::integers::<u8>().max_value(6)) {
                0 => NodeState::Waiting,
                1 => NodeState::Ready,
                2 => NodeState::Running,
                3 => NodeState::Built,
                4 => NodeState::Cached { remote: false },
                5 => NodeState::Failed { why: "refused".into() },
                _ => NodeState::Skipped { because: 0 },
            }
        };
        deps.push(mine);
        states.push(state);
    }

    let mut fold = Fold::new();
    fold.advance_clock(0);
    let nodes: Vec<(NodeKind, String, Vec<u32>)> = deps
        .iter()
        .enumerate()
        .map(|(id, it)| (NodeKind::Bodies, format!("t{id}"), it.clone()))
        .collect();
    fold.take_event(&batch(0, &nodes));
    for (id, state) in states.iter().enumerate() {
        fold.advance_clock((id as u64 + 1) * S);
        if *state != NodeState::Waiting {
            moved(&mut fold, id as u32, NodeState::Ready);
            if *state != NodeState::Ready {
                moved(&mut fold, id as u32, state.clone());
            }
        }
    }
    let memory = Memory::default();
    let now = fold.look(&look(&[], &[], &memory));

    let unfinished = |id: usize| !states[id].settled();
    let frontier: Vec<usize> = (0..len)
        .filter(|&id| unfinished(id) && deps[id].iter().all(|&dep| states[dep as usize].settled()))
        .collect();
    if frontier.len() > BLOCKERS {
        return;
    }
    // Each task's frontier tasks, as bits by id; the longest path behind each.
    let mut sets = vec![0u32; len];
    for id in (0..len).filter(|&id| unfinished(id)) {
        if frontier.contains(&id) {
            sets[id] = 1 << id;
        } else {
            for &dep in &deps[id] {
                sets[id] |= sets[dep as usize];
            }
        }
    }
    let mut down = vec![0u32; len];
    for id in (0..len).rev().filter(|&id| unfinished(id)) {
        for other in (id + 1..len).filter(|&other| unfinished(other)) {
            if deps[other].contains(&(id as u32)) {
                down[id] = down[id].max(down[other] + 1);
            }
        }
    }
    let off: Vec<usize> =
        (0..len).filter(|&id| unfinished(id) && !frontier.contains(&id)).collect();

    let mut ids: Vec<usize> = now.blockers.iter().map(|it| it.task.id as usize).collect();
    ids.sort_unstable();
    assert_eq!(ids, frontier, "{now:?}");
    let rank =
        |it: &Blocker| (Reverse(it.holds), Reverse(it.chain), Reverse(it.for_ms), it.task.id);
    assert!(now.blockers.is_sorted_by_key(rank), "{:?}", now.blockers);
    for blocker in &now.blockers {
        let at = blocker.task.id as usize;
        let held = off.iter().filter(|&&id| sets[id] & (1 << at) != 0).count() as u32;
        assert_eq!((blocker.holds, blocker.chain), (held, down[at]), "{blocker:?} of {deps:?}");
    }
    for &id in &off {
        let held = now.blockers.iter().any(|it| it.holds > 0 && sets[id] & (1 << it.task.id) != 0);
        assert!(held, "task {id} is held by no blocker: {now:?}");
    }
    let holds: u32 = now.blockers.iter().map(|it| it.holds).sum();
    assert!(holds as usize >= off.len(), "{holds} held of {} off the frontier", off.len());
}
