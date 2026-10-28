use blaze_protocol::events::{
    Cache, Cause, CrateState, Event, Fns, NodeKind, NodeState, Record, Slow, Step, Summary,
};

use crate::{Live, Plain, summary_line, took};

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn at(at_ns: u64, event: Event) -> Record {
    Record { at_ns, event }
}

fn anim(state: CrateState, done: u64, kept: u64) -> Event {
    Event::Crate { name: "anim".into(), state, fns: Fns { total: 5353, done, kept } }
}

/// Andrew's `blaze run` of anim from cold (04:51Z): 38 s loading, 3 s resolving, 33 s
/// compiling.
fn cold() -> Vec<Record> {
    vec![
        at(0, Event::Began { build: 1, command: "run".into(), root: "/w/anim".into() }),
        at(0, Event::Started { step: Step::LoadWorkspace, of: None }),
        at(38_300 * MS, Event::Ended { step: Step::LoadWorkspace, of: None }),
        at(38_300 * MS, Event::Started { step: Step::Resolve, of: None }),
        at(41_500 * MS, Event::Ended { step: Step::Resolve, of: None }),
        at(41_500 * MS, Event::Started { step: Step::Codegen, of: None }),
        at(41_500 * MS, anim(CrateState::Running, 0, 0)),
        at(50 * S, anim(CrateState::Running, 1200, 2)),
        at(74_800 * MS, anim(CrateState::Built, 5353, 2)),
        at(74_800 * MS, Event::Ended { step: Step::Codegen, of: None }),
        at(74_800 * MS, Event::Started { step: Step::Link, of: Some("anim".into()) }),
        at(74_900 * MS, Event::Ended { step: Step::Link, of: Some("anim".into()) }),
        at(
            74_900 * MS,
            Event::Finished {
                ok: true,
                summary: Summary {
                    took_ns: 74_900 * MS,
                    crates: 1,
                    fns: Fns { total: 5353, done: 5353, kept: 2 },
                    ..Summary::default()
                },
            },
        ),
    ]
}

/// An edit of `render`'s body, which `draw` and `tick` call by its code.
fn edit() -> Vec<Record> {
    let rebuilt = |name: &str, cause: Cause| Event::Rebuilt {
        name: name.into(),
        krate: "anim".into(),
        cause,
    };
    let callee = || Cause::Callee { name: "render".into() };
    vec![
        at(0, Event::Started { step: Step::Codegen, of: None }),
        at(20 * MS, anim(CrateState::Running, 0, 0)),
        at(90 * MS, rebuilt("render", Cause::Body)),
        at(120 * MS, rebuilt("draw", callee())),
        at(150 * MS, rebuilt("tick", callee())),
        at(180 * MS, anim(CrateState::Built, 5353, 5350)),
        at(180 * MS, Event::Ended { step: Step::Codegen, of: None }),
        at(
            210 * MS,
            Event::Finished {
                ok: true,
                summary: Summary {
                    took_ns: 210 * MS,
                    crates: 1,
                    fns: Fns { total: 5353, done: 5353, kept: 5350 },
                    rebuilt: vec![
                        ("render".into(), Cause::Body),
                        ("draw".into(), callee()),
                        ("tick".into(), callee()),
                    ],
                    ..Summary::default()
                },
            },
        ),
    ]
}

#[test]
fn durations_read_as_people_say_them() {
    assert_eq!(took(210 * MS), "0.21s");
    assert_eq!(took(3_200 * MS), "3.20s");
    assert_eq!(took(33_300 * MS), "33.3s");
    assert_eq!(took(77 * S), "1m17s");
}

#[test]
fn a_cold_build_off_a_terminal_is_a_line_per_step() {
    let mut plain = Plain::new(false);
    let mut out = String::new();
    for record in cold() {
        // What a log shows while the 33 s of compiling go by.
        if record.at_ns == 74_800 * MS {
            out.push_str(&plain.tick(60 * S));
            out.push_str(&plain.tick(61 * S));
        }
        out.push_str(&plain.record(&record));
    }
    let expected = [
        "     Loading workspace",
        "      Loaded workspace in 38.3s",
        "   Resolving names",
        "    Resolved names in 3.20s",
        "   Compiling anim",
        "   Compiling functions",
        "    Compiled anim: 5351 fns compiled, 2 kept in 33.3s",
        "    Compiled 5351 fns in 1 crate, 2 kept; 1m14s",
    ];
    assert_eq!(out, expected.map(|it| format!("{it}\n")).concat());
}

#[test]
fn an_edit_says_what_it_rebuilt_and_why() {
    let mut plain = Plain::new(false);
    let out: String = edit().iter().map(|it| plain.record(it)).collect();
    assert_eq!(
        out,
        "   Compiling anim\n     Rebuilt 3 fns: render (body edited), draw, tick (calls render); \
         0.21s\n"
    );
    // `blaze status` of it: the steps it did and its last line, as the live view ends.
    let mut live = Live::new(100, false);
    for record in edit() {
        live.record(&record, record.at_ns);
    }
    assert_eq!(
        live.snapshot(live.build().last_ns),
        [
            "\u{2713} codegen 0.18s",
            "     Rebuilt 3 fns: render (body edited), draw, tick (calls render); 0.21s",
        ]
    );
    let finished = plain.build().finished.clone().unwrap();
    assert!(summary_line(&finished, true).contains("\x1b["));
}

#[test]
fn a_fresh_build_says_nothing_of_its_own() {
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/anim".into() }),
        at(MS, Event::Finished { ok: true, summary: Summary::default() }),
    ];
    let mut plain = Plain::new(false);
    assert_eq!(records.iter().map(|it| plain.record(it)).collect::<String>(), "");
    let mut live = Live::new(80, false);
    let drawn: String = records.iter().map(|it| live.record(it, MS)).collect();
    assert!(!drawn.contains("Fresh"), "{drawn:?}");
}

/// A failed build's last line says why to who watches it; the command that asked for it says
/// the errors in full and cargo's `could not compile` after, so it has no line of its own,
/// as cargo has one line of failure.
#[test]
fn a_failure_is_said_once_to_the_command_that_asked() {
    let error = "error[E0308]: expected u32, found &'static str".to_owned();
    let summary = Summary { took_ns: 40 * MS, error: Some(error), ..Summary::default() };
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/anim".into() }),
        at(40 * MS, Event::Finished { ok: false, summary }),
    ];
    let mut watched = Plain::new(false);
    let said: String = records.iter().map(|it| watched.record(it)).collect();
    assert_eq!(said, "error: error[E0308]: expected u32, found &'static str (after 0.04s)\n");
    let mut asked = Plain::new(false).errors_said();
    assert_eq!(records.iter().map(|it| asked.record(it)).collect::<String>(), "");
    let mut live = Live::new(80, false).errors_said();
    let drawn: String = records.iter().map(|it| live.record(it, 40 * MS)).collect();
    assert!(!drawn.contains("error"), "{drawn:?}");
}

/// On a terminal: the region is redrawn in place, each of its lines fits the width, each
/// redraw clears exactly what was drawn, and at the end the summary is all that is left
/// of it.
#[test]
fn the_live_region_is_redrawn_in_place_and_cleared() {
    let width = 60;
    let mut live = Live::new(width, false);
    let mut drawn = 0;
    let mut screens = Vec::new();
    let mut time = 0;
    for record in cold() {
        // Ticks every 100 ms between records: never more than that without a redraw.
        while time + 100 * MS < record.at_ns {
            time += 100 * MS;
            screens.push((time, live.tick(time)));
        }
        screens.push((record.at_ns, live.record(&record, record.at_ns)));
    }
    for (time, screen) in &screens {
        let mut rest = screen.as_str();
        if drawn > 0 {
            let clear = format!("\r\x1b[{drawn}A\x1b[J");
            rest = rest.strip_prefix(&clear).unwrap_or_else(|| panic!("{time}: {screen:?}"));
        }
        assert!(!rest.contains('\x1b'), "{time}: {rest:?}");
        let lines: Vec<&str> = rest.lines().collect();
        for line in &lines {
            assert!(line.chars().count() <= width, "{time}: {line:?}");
        }
        drawn = if live_finished(rest) { 0 } else { region_len(&lines) };
    }
    let last = &screens.last().unwrap().1;
    assert!(last.ends_with("    Compiled 5351 fns in 1 crate, 2 kept; 1m14s\n"), "{last:?}");
    // In the middle of the compile: the step, the crate under way, its count.
    let middle = &screens.iter().find(|it| it.0 == 60 * S).unwrap().1;
    let region: Vec<&str> = middle.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        region,
        [
            format!("blaze run{:45}1m00s", "").as_str(),
            "\u{2713} workspace 38.3s  \u{2713} names 3.20s  \u{25cf} codegen 18.5s",
            "   Compiling functions 18.5s \u{b7} 0/1 crates \u{b7} 1200/5353 fns",
            "            \u{2514}\u{2500} anim  1200/5353 fns  18.5s",
        ]
    );
}

fn crate_is(name: &str, state: CrateState, done: u64) -> Event {
    Event::Crate { name: name.into(), state, fns: Fns { total: 0, done, kept: 0 } }
}

fn node(id: u32, kind: NodeKind, name: &str, deps: &[u32]) -> Event {
    Event::Node { id, kind, name: name.into(), deps: deps.to_vec() }
}

/// anim's graph, cut down: the standard library, libc, two crates that use it and anim.
fn graph() -> Vec<Event> {
    vec![
        node(10, NodeKind::Std, "std", &[]),
        node(11, NodeKind::Names, "libc", &[10]),
        node(12, NodeKind::Names, "winit", &[10, 11]),
        node(13, NodeKind::Names, "wgpu", &[10, 11]),
        node(14, NodeKind::Names, "anim", &[12, 13]),
    ]
}

fn graphed() -> Vec<Record> {
    vec![
        at(0, Event::Began { build: 1, command: "run".into(), root: "/w/anim".into() }),
        at(0, Event::Started { step: Step::Resolve, of: None }),
    ]
    .into_iter()
    .chain(graph().into_iter().map(|it| at(0, it)))
    .chain([
        at(100 * MS, crate_is("std", CrateState::Resolving, 0)),
        at(S, crate_is("std", CrateState::Resolved, 0)),
        at(S, crate_is("libc", CrateState::Resolving, 0)),
        at(1500 * MS, crate_is("libc", CrateState::Resolved, 0)),
        at(1500 * MS, crate_is("winit", CrateState::Resolving, 0)),
        at(1500 * MS, crate_is("wgpu", CrateState::Resolving, 0)),
        at(
            2 * S,
            Event::Progress {
                step: Step::Resolve,
                done: 2,
                total: 5,
                longest: Some("winit".into()),
            },
        ),
    ])
    .collect()
}

/// With the graph of its crates, the region is a tree: what is under way, below what waits
/// for it, each with what it is doing and for how long; what is done or waits is counted in
/// the last line, with the longest chain so far.
#[test]
fn the_region_is_a_tree_of_the_crates_under_way() {
    let mut live = Live::new(120, false);
    let mut drawn = String::new();
    for record in graphed() {
        drawn = live.record(&record, record.at_ns);
    }
    drawn = live.tick(3 * S);
    let region: Vec<&str> = drawn.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        region,
        [
            format!("blaze run{:105}3.00s", "").as_str(),
            "\u{25cf} names 2/5 crates 3.00s",
            "   Resolving names 3.00s \u{b7} 2/5 crates \u{b7} longest winit",
            "             anim waiting",
            "             \u{251c}\u{2500} winit  resolving  1.50s",
            "             \u{2514}\u{2500} wgpu  resolving  1.50s",
            "             0 done \u{b7} 2 under way \u{b7} 3 waiting",
        ]
    );
}

/// The driver's nodes that are no crate's (docs/driver.md) are lines of their own, moved by
/// their [`Event::Moved`]s: a build script compiling waits for nothing, its run for it.
#[test]
fn a_build_script_is_a_node_of_its_own() {
    let mut live = Live::new(120, false);
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/rg".into() }),
        at(0, Event::Started { step: Step::BuildScripts, of: None }),
        at(0, node(0, NodeKind::Script, "anyhow", &[])),
        at(0, node(1, NodeKind::ScriptRun, "anyhow", &[0])),
        at(0, node(2, NodeKind::Macro, "serde_derive", &[])),
        at(100 * MS, Event::Moved { id: 0, state: NodeState::Running }),
        at(100 * MS, Event::Moved { id: 2, state: NodeState::Running }),
        at(900 * MS, Event::Moved { id: 2, state: NodeState::Cached { remote: false } }),
    ];
    for record in &records {
        live.record(record, record.at_ns);
    }
    let drawn = live.tick(2 * S);
    let region: Vec<&str> = drawn.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        region,
        [
            format!("blaze build{:103}2.00s", "").as_str(),
            "\u{25cf} build scripts 2.00s",
            "    Building build scripts and proc macros 2.00s",
            "             anyhow build script  compiling  1.90s",
            "             1 done (1 up to date) \u{b7} 1 under way \u{b7} 0 waiting",
        ]
    );
}

/// A build that took long ends saying why: its slowest steps, and what the store had and,
/// of a cold one, why it had nothing.
#[test]
fn a_long_build_says_why_it_took_as_long_as_it_did() {
    let mut build = crate::Build::default();
    let steps = [
        (Step::LoadWorkspace, None, 0, 10_700),
        (Step::Resolve, None, 10_700, 13_100),
        (Step::Codegen, Some("anim"), 13_100, 17_300),
        (Step::Check, Some("14 files"), 17_300, 117_300),
        (Step::Link, Some("anim"), 117_300, 117_400),
    ];
    for (step, of, began, ended) in steps {
        let of: Option<String> = of.map(Into::into);
        build.apply(&at(began * MS, Event::Started { step, of: of.clone() }));
        build.apply(&at(ended * MS, Event::Ended { step, of }));
    }
    let cold = Some("cache keys changed since blaze 0.20261027.0 (fb82b09)".into());
    build.apply(&at(
        17 * S,
        Event::Cache { cache: Cache { found: 0, fetched: 0, compiled: 77917, cold } },
    ));
    build.apply(&at(17 * S, crate_is("anim", CrateState::Built, 77917)));
    let summary = build.summary(117_400 * MS, None);
    let (chain, chain_ns, unnamed, cores) = (Vec::new(), 0, None, Vec::new());
    let finished = crate::Finished {
        ok: true,
        summary,
        at_ns: 117_400 * MS,
        chain,
        chain_ns,
        unnamed,
        cores,
        cores_of: 0,
        slowest: Vec::new(),
    };
    let lines = crate::last_lines(&finished, false, true);
    assert_eq!(
        lines,
        [
            "    Compiled 77917 fns in 1 crate, 0 kept; 1m57s",
            "        Took 1m57s: check 14 files 1m40s, workspace 10.7s, codegen anim 4.20s",
            "       Cache cold: cache keys changed since blaze 0.20261027.0 (fb82b09)",
            "             `blaze why` says more",
        ]
    );
    // A quick build says nothing more than it did.
    let quick =
        crate::Finished { summary: build.summary(400 * MS, None), at_ns: 400 * MS, ..finished };
    assert_eq!(crate::last_lines(&quick, false, true).len(), 1);
}

/// A build that ends says what held it up, as its graph says it, and time that went by with
/// no step named, which is a step that is missing.
#[test]
fn a_long_build_says_what_held_it_up_and_time_no_step_named() {
    let mut live = Live::new(120, false);
    let records = graphed().into_iter().chain([
        at(2 * S, crate_is("winit", CrateState::Resolved, 0)),
        at(2 * S, crate_is("wgpu", CrateState::Resolved, 0)),
        at(2 * S, Event::Ended { step: Step::Resolve, of: None }),
        at(3500 * MS, Event::Started { step: Step::Link, of: None }),
        at(4 * S, Event::Ended { step: Step::Link, of: None }),
        at(4 * S, Event::Critical { path: vec![10, 11, 13] }),
        at(4 * S, Event::Finished { ok: true, summary: Summary::default() }),
    ]);
    let drawn: String = records.map(|it| live.record(&it, it.at_ns)).collect();
    let end: Vec<&str> = drawn.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        end[1..],
        [
            "       Chain std \u{2192} libc \u{2192} wgpu 1.90s",
            "warning: 1.50s with no step named, after names",
            "             `blaze why` says more",
        ]
    );
    // `blaze why` lists every step, and the time at no step between them.
    assert_eq!(
        live.build().why_steps(4 * S),
        ["names 2/5 crates   2.00s", "at no step         1.50s", "link               0.50s"]
    );
}

/// How busy the worker's cores were shows as it goes, the last ten seconds a block each half
/// second, and the end says how many cores each slow step kept busy: a step on one core of
/// eight is why a build is slow.
#[test]
fn the_cores_kept_busy_show_as_it_goes_and_at_the_end() {
    let took = crate::Took { step: Step::Std, of: None, took_ns: 4 * S };
    let summary = Summary { took_ns: 4 * S, slowest: vec![took], ..Summary::default() };
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/rg".into() }),
        at(0, Event::Started { step: Step::Std, of: None }),
        at(500 * MS, Event::Cores { busy: 100, of: 8 }),
        at(3 * S, Event::Cores { busy: 790, of: 8 }),
    ];
    let mut live = Live::new(120, false);
    for record in &records {
        live.record(record, record.at_ns);
    }
    let drawn = live.tick(4 * S);
    let region: Vec<&str> = drawn.split("\x1b[J").last().unwrap().lines().collect();
    let spark = format!(
        "cores {:12}\u{2582}\u{2582}\u{2582}\u{2582}\u{2582}\u{2588}\u{2588}\u{2588} 7.9/8 busy",
        ""
    );
    assert_eq!(region[2], spark);
    let end = [
        at(4 * S, Event::Ended { step: Step::Std, of: None }),
        at(4 * S, Event::Finished { ok: true, summary }),
    ];
    let drawn: String = end.iter().map(|it| live.record(it, it.at_ns)).collect();
    assert!(drawn.contains("Took 4.00s: std 4.00s (2.7/8 cores)"), "{drawn:?}");
    assert_eq!(live.build().why_steps(4 * S), ["std   4.00s  2.7/8 cores"]);
}

/// Under each crate under way, the functions of it compiling longest so far, each over a
/// moment; at the end the slowest five.
#[test]
fn the_functions_slow_to_compile_show_under_their_crate_and_at_the_end() {
    let slow = |name: &str, ns| Slow { krate: "wgpu".into(), name: name.into(), ns };
    let compiling = Event::Compiling {
        functions: vec![slow("wgpu::Device::new", 300 * MS), slow("wgpu::Queue::submit", 100 * MS)],
    };
    let mut live = Live::new(120, false);
    for record in graphed().into_iter().chain([at(2 * S, compiling)]) {
        live.record(&record, record.at_ns);
    }
    let drawn = live.tick(3 * S);
    let region: Vec<&str> = drawn.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        region[4..],
        [
            "             \u{251c}\u{2500} winit  resolving  1.50s",
            "             \u{2514}\u{2500} wgpu  resolving  1.50s",
            format!("{:18}wgpu::Device::new 1.30s", "").as_str(),
            format!("{:18}wgpu::Queue::submit 1.10s", "").as_str(),
            "             0 done \u{b7} 2 under way \u{b7} 3 waiting",
        ]
    );
    let slowest =
        vec![slow("wgpu::Device::new", 1400 * MS), slow("wgpu::Queue::submit", 1100 * MS)];
    let end = [
        at(3 * S, Event::Ended { step: Step::Resolve, of: None }),
        at(3 * S, Event::Slowest { functions: slowest }),
        at(3 * S, Event::Finished { ok: true, summary: Summary::default() }),
    ];
    let drawn: String = end.iter().map(|it| live.record(it, it.at_ns)).collect();
    let said = "     Slowest fns: wgpu::Device::new 1.40s, wgpu::Queue::submit 1.10s";
    assert!(drawn.contains(said), "{drawn:?}"); // A name too long keeps its end, which says which function it is.
    let name = "<regex_syntax::unicode::SimpleCaseFolder>::new";
    assert_eq!(crate::tail(name, 24), "\u{2026}:SimpleCaseFolder>::new");
}

/// The functions done of a crate whose total is not known yet count in no `done/total`, which
/// would read more done than there are.
#[test]
fn functions_with_a_total_unknown_are_a_count() {
    let mut live = Live::new(120, false);
    for record in [
        at(0, Event::Started { step: Step::Codegen, of: None }),
        at(0, anim(CrateState::Running, 1200, 0)),
        at(0, crate_is("std", CrateState::Running, 6275)),
    ] {
        live.record(&record, record.at_ns);
    }
    let drawn = live.tick(S);
    assert!(drawn.contains(" \u{b7} 7475 fns"), "{drawn}");
    assert!(!drawn.contains("/5353 fns\n"), "{drawn}");
}

/// The lines that stay are those before the region: the region is its header, the steps,
/// the crates under way and the count of those done. A test of the renderer's own text, with
/// no color.
fn region_len(lines: &[&str]) -> usize {
    let header = lines.iter().rposition(|it| it.starts_with("blaze "));
    header.map_or(0, |at| lines.len() - at)
}

fn live_finished(rest: &str) -> bool {
    rest.contains("    Compiled 5351 fns in 1 crate")
}
