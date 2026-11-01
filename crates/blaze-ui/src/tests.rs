use std::hash::BuildHasher;

use blaze_hash::FixedState;
use blaze_ids::rng::SplitMix64;
use blaze_protocol::events::{
    Cache, Cause, CrateState, Event, Fns, Level, NodeKind, NodeState, Record, Slow, Step, Summary,
    Took,
};

use crate::{
    Build, Count, Dur, Finished, Live, Plain, Progress, Unnamed, summary_line, took, why_lines,
};

mod old;

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn at(at_ns: u64, event: Event) -> Record {
    Record { at_ns, event }
}

/// A view that draws for every record and tick, so a test sees the screen after each.
fn every_record(width: usize, color: bool) -> Live {
    Live::new(width, color).frames(0)
}

fn anim(state: CrateState, done: u64, kept: u64) -> Event {
    Event::Crate { name: "anim".into(), state, fns: Fns { total: 5353, done, kept } }
}

/// A terminal as far as the renderers drive it: rows of cells and a cursor, moved by `\n`,
/// `\r`, cursor up and down, cleared by `\x1b[J` and `\x1b[K`/`\x1b[2K`. A cell is its
/// character with the colors written before it, and those written after the last of a row.
/// It never scrolls: a row above the first is a renderer's mistake, and panics.
#[derive(Default)]
struct Screen {
    rows: Vec<Vec<String>>,
    row: usize,
    col: usize,
    colors: String,
}

impl Screen {
    fn feed(&mut self, bytes: &str) {
        let mut chars = bytes.chars();
        while let Some(char) = chars.next() {
            match char {
                '\x1b' => {
                    assert_eq!(chars.next(), Some('['), "{bytes:?}");
                    let (mut sequence, mut number) = ("\x1b[".to_owned(), String::new());
                    let end = loop {
                        let char = chars.next().unwrap_or_else(|| panic!("{bytes:?}"));
                        sequence.push(char);
                        match char.is_ascii_digit() {
                            true => number.push(char),
                            false => break char,
                        }
                    };
                    let number: Option<usize> = number.parse().ok();
                    if end == 'm' {
                        self.colors.push_str(&sequence);
                        continue;
                    }
                    self.settle();
                    match (end, number) {
                        ('A', Some(up)) => {
                            let row = self.row;
                            assert!(up > 0 && up <= row, "up {up} from {row}: {bytes:?}");
                            self.row -= up;
                        }
                        ('B', Some(down)) => {
                            assert!(down > 0, "{bytes:?}");
                            self.row += down;
                            assert!(self.row <= self.rows.len(), "down past the end: {bytes:?}");
                        }
                        // A row cleared whole is no row: a terminal shows them the same.
                        ('J', None) if self.col == 0 => self.rows.truncate(self.row),
                        ('J', None) => {
                            self.rows.truncate(self.row + 1);
                            if let Some(row) = self.rows.get_mut(self.row) {
                                row.truncate(self.col);
                            }
                        }
                        ('K', None) => {
                            if let Some(row) = self.rows.get_mut(self.row) {
                                row.truncate(self.col);
                            }
                        }
                        ('K', Some(2)) => {
                            if let Some(row) = self.rows.get_mut(self.row) {
                                row.clear();
                            }
                        }
                        _ => panic!("{sequence:?} in {bytes:?}"),
                    }
                }
                '\n' => {
                    self.settle();
                    self.row += 1;
                    self.col = 0;
                }
                '\r' => {
                    self.settle();
                    self.col = 0;
                }
                char => {
                    if self.rows.len() <= self.row {
                        self.rows.resize_with(self.row + 1, Vec::new);
                    }
                    let row = &mut self.rows[self.row];
                    if row.len() < self.col {
                        row.resize(self.col, " ".to_owned());
                    }
                    let mut cell = std::mem::take(&mut self.colors);
                    cell.push(char);
                    match row.get_mut(self.col) {
                        Some(it) => *it = cell,
                        None => row.push(cell),
                    }
                    self.col += 1;
                }
            }
        }
        self.settle();
    }

    /// Colors written after the last character of a row go with it.
    fn settle(&mut self) {
        if self.colors.is_empty() || self.col == 0 {
            return;
        }
        if let Some(cell) = self.rows.get_mut(self.row).and_then(|it| it.get_mut(self.col - 1)) {
            cell.push_str(&std::mem::take(&mut self.colors));
        }
    }

    fn lines(&self) -> Vec<String> {
        self.rows.iter().map(|it| it.concat()).collect()
    }

    /// The `drawn` rows above the cursor: the region, when a renderer drew it last.
    fn region(&self, drawn: usize) -> Vec<String> {
        self.lines()[self.row - drawn..self.row].to_vec()
    }
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

/// The widths the region is laid out by are counted, not measured: each is what is written.
#[test]
fn the_lengths_counted_are_the_lengths_written() {
    let times = [0, 1, 9_999 * MS, 10 * S, 59_999 * MS, 60 * S, 3_599 * S, 600_000 * S, u64::MAX];
    for ns in times {
        assert_eq!(Dur(ns).chars(), took(ns).chars().count(), "{ns}");
    }
    for (done, total) in [(0, 0), (9, 0), (10, 0), (1200, 5353), (0, 1), (u64::MAX, u64::MAX)] {
        let progress = Progress { done, total, longest: None };
        for step in STEPS {
            let count = Count(&progress, step);
            assert_eq!(count.chars(), count.to_string().chars().count(), "{done}/{total}");
        }
    }
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
    let mut live = every_record(100, false);
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
    let finished = plain.build().finished.as_ref().unwrap();
    assert!(summary_line(finished, true).contains("\x1b["));
}

#[test]
fn a_fresh_build_says_nothing_of_its_own() {
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/anim".into() }),
        at(MS, Event::Finished { ok: true, summary: Summary::default() }),
    ];
    let mut plain = Plain::new(false);
    assert_eq!(records.iter().map(|it| plain.record(it)).collect::<String>(), "");
    let mut live = every_record(80, false);
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
    let mut live = every_record(80, false).errors_said();
    let drawn: String = records.iter().map(|it| live.record(it, 40 * MS)).collect();
    assert!(!drawn.contains("error"), "{drawn:?}");
}

/// On a terminal: the region is redrawn in place, each of its lines fits the width, and at
/// the end the summary is all that is left of it.
#[test]
fn the_live_region_is_redrawn_in_place_and_cleared() {
    let width = 60;
    let mut live = every_record(width, false);
    let mut screen = Screen::default();
    let mut middle = Vec::new();
    let mut time = 0;
    let fits = |screen: &Screen| {
        for line in screen.lines() {
            assert!(line.chars().count() <= width, "{line:?}");
        }
    };
    for record in cold() {
        // Ticks every 100 ms between records: never more than that without a redraw.
        while time + 100 * MS < record.at_ns {
            time += 100 * MS;
            screen.feed(&live.tick(time));
            fits(&screen);
            if time == 60 * S {
                middle = screen.region(live.drawn());
            }
        }
        screen.feed(&live.record(&record, record.at_ns));
        fits(&screen);
    }
    assert_eq!(live.drawn(), 0);
    assert_eq!(
        screen.lines().last().unwrap(),
        "    Compiled 5351 fns in 1 crate, 2 kept; 1m14s"
    );
    // In the middle of the compile: the step, the crate under way, its count.
    assert_eq!(
        middle,
        [
            format!("blaze run{:45}1m00s", "").as_str(),
            "\u{2713} workspace 38.3s  \u{2713} names 3.20s  \u{25cf} codegen 18.5s",
            "   Compiling functions 18.5s \u{b7} 0/1 crates \u{b7} 1200/5353 fns",
            "            \u{2514}\u{2500} anim  1200/5353 fns  18.5s",
        ]
    );
}

/// A draw writes only what changed of the region: nothing when nothing did, and a line that
/// changed with the cursor moved to it and back. One whose width changed is drawn whole.
#[test]
fn a_draw_writes_only_the_lines_that_changed() {
    let mut live = every_record(120, false);
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/rg".into() }),
        at(0, Event::Started { step: Step::Std, of: None }),
        at(500 * MS, Event::Cores { busy: 100, of: 8 }),
    ];
    for record in &records {
        live.record(record, S);
    }
    assert_eq!(live.drawn(), 4);
    assert_eq!(live.tick(S), "");
    let drawn = live.record(&at(S, Event::Cores { busy: 790, of: 8 }), S);
    let spark = format!("{:18}\u{2582}\u{2588} 7.9/8 busy", "");
    assert_eq!(drawn, format!("\r\x1b[2A\x1b[2Kcores {spark}\n\x1b[1B"));
    live.resize(100);
    assert!(live.tick(S).starts_with("\r\x1b[4A\x1b[J"));
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

/// Draws `records` on a screen, each at its time, then a tick at `now_ns`: the region then.
fn region_at(
    live: &mut Live,
    records: impl IntoIterator<Item = Record>,
    now_ns: u64,
) -> Vec<String> {
    let mut screen = Screen::default();
    for record in records {
        screen.feed(&live.record(&record, record.at_ns));
    }
    screen.feed(&live.tick(now_ns));
    screen.region(live.drawn())
}

/// With the graph of its crates, the region is a tree: what is under way, below what waits
/// for it, each with what it is doing and for how long; what is done or waits is counted in
/// the last line, with the longest chain so far.
#[test]
fn the_region_is_a_tree_of_the_crates_under_way() {
    let mut live = every_record(120, false);
    let region = region_at(&mut live, graphed(), 3 * S);
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
    let mut live = every_record(120, false);
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
    let region = region_at(&mut live, records, 2 * S);
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

/// The [`Event::Nodes`] of nodes given as their [`Event::Node`]s would say them, the ids from
/// `first` on.
fn nodes(first: u32, given: &[(NodeKind, &str, &[u32])]) -> Event {
    let (mut names, mut name_at) = (String::new(), vec![0]);
    let (mut deps, mut deps_at) = (Vec::new(), vec![0]);
    for (_, name, on) in given {
        names.push_str(name);
        name_at.push(names.len() as u32);
        deps.extend_from_slice(on);
        deps_at.push(deps.len() as u32);
    }
    let kinds = given.iter().map(|it| it.0).collect();
    Event::Nodes { first, kinds, names, name_at, deps, deps_at }
}

/// Nodes said in one record are the nodes said a record each: anim's graph so, given in
/// order or each before what it waits for, is the same graph and draws the same tree. A
/// record of nodes that cannot be read is said as an error, and adds no node.
#[test]
fn a_record_of_nodes_is_its_nodes() {
    use NodeKind::{Names, Std};
    let with = |nodes: Event| -> Vec<Record> {
        let mut records = graphed();
        records.retain(|it| !matches!(it.event, Event::Node { .. }));
        records.insert(2, at(0, nodes));
        records
    };
    let in_order = with(nodes(
        10,
        &[
            (Std, "std", &[]),
            (Names, "libc", &[10]),
            (Names, "winit", &[10, 11]),
            (Names, "wgpu", &[10, 11]),
            (Names, "anim", &[12, 13]),
        ],
    ));
    let backwards = with(nodes(
        10,
        &[
            (Names, "anim", &[11, 12]),
            (Names, "winit", &[14, 13]),
            (Names, "wgpu", &[14, 13]),
            (Names, "libc", &[14]),
            (Std, "std", &[]),
        ],
    ));
    let shape = |records: &[Record]| {
        let mut build = Build::default();
        for record in records {
            build.apply(record);
        }
        let row = |at: usize| build.crates[build.graph[at].row as usize].name.clone();
        let deps =
            |at: usize| -> Vec<String> { build.deps(at).iter().map(|&it| row(it as usize)).collect() };
        (0..build.graph.len())
            .map(|at| (row(at), build.graph[at].kind, deps(at)))
            .collect::<Vec<(String, NodeKind, Vec<String>)>>()
    };
    let each = graphed();
    assert_eq!(shape(&in_order), shape(&each));
    assert_eq!(shape(&backwards), shape(&each));
    let region = region_at(&mut every_record(120, false), each, 3 * S);
    assert_eq!(region_at(&mut every_record(120, false), in_order, 3 * S), region);
    assert_eq!(region_at(&mut every_record(120, false), backwards, 3 * S), region);

    let mut plain = Plain::new(false);
    let cycle = nodes(0, &[(Names, "a", &[1]), (Names, "b", &[0])]);
    let said = plain.record(&at(0, cycle));
    assert_eq!(said, "error: a record of nodes is malformed: nodes 0, 1 wait for each other\n");
    let Event::Nodes { first, kinds, names, name_at, deps, .. } = nodes(0, &[(Std, "std", &[])])
    else {
        unreachable!("an Event::Nodes")
    };
    let cut = Event::Nodes { first, kinds, names, name_at, deps, deps_at: vec![0] };
    let said = plain.record(&at(0, cut));
    assert!(said.starts_with("error: a record of nodes is malformed: deps_at of 1 offsets"));
    assert!(plain.build().graph.is_empty());
    assert_eq!(plain.build().malformed.len(), 2);
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
    let mut live = every_record(120, false);
    let records = graphed().into_iter().chain([
        at(2 * S, crate_is("winit", CrateState::Resolved, 0)),
        at(2 * S, crate_is("wgpu", CrateState::Resolved, 0)),
        at(2 * S, Event::Ended { step: Step::Resolve, of: None }),
        at(3500 * MS, Event::Started { step: Step::Link, of: None }),
        at(4 * S, Event::Ended { step: Step::Link, of: None }),
        at(4 * S, Event::Critical { path: vec![10, 11, 13] }),
        at(4 * S, Event::Finished { ok: true, summary: Summary::default() }),
    ]);
    let mut screen = Screen::default();
    for record in records {
        screen.feed(&live.record(&record, record.at_ns));
    }
    let lines = screen.lines();
    assert_eq!(
        lines[lines.len() - 3..],
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
    let mut live = every_record(120, false);
    let region = region_at(&mut live, records, 4 * S);
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
    let mut live = every_record(120, false);
    let records = graphed().into_iter().chain([at(2 * S, compiling)]);
    let region = region_at(&mut live, records, 3 * S);
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
    assert!(drawn.contains(said), "{drawn:?}");
    // A name too long keeps its end, which says which function it is.
    let name = "<regex_syntax::unicode::SimpleCaseFolder>::new";
    assert_eq!(crate::tail(name, 24), "\u{2026}:SimpleCaseFolder>::new");
}

/// The functions done of a crate whose total is not known yet count in no `done/total`, which
/// would read more done than there are.
#[test]
fn functions_with_a_total_unknown_are_a_count() {
    let mut live = every_record(120, false);
    let records = [
        at(0, Event::Started { step: Step::Codegen, of: None }),
        at(0, anim(CrateState::Running, 1200, 0)),
        at(0, crate_is("std", CrateState::Running, 6275)),
    ];
    let region = region_at(&mut live, records, S);
    assert!(region[2].ends_with(" \u{b7} 7475 fns"), "{region:?}");
    assert!(!region.iter().any(|it| it.ends_with("/5353 fns")), "{region:?}");
}

const STEPS: [Step; 15] = [
    Step::LoadWorkspace,
    Step::Fetch,
    Step::Resolve,
    Step::FrontEnd,
    Step::Codegen,
    Step::Std,
    Step::Link,
    Step::Check,
    Step::Run,
    Step::Test,
    Step::BuildScripts,
    Step::Macros,
    Step::Optimize,
    Step::Store,
    Step::Remote,
];

const KINDS: [NodeKind; 12] = [
    NodeKind::Manifest,
    NodeKind::Fetch,
    NodeKind::Script,
    NodeKind::ScriptRun,
    NodeKind::Macro,
    NodeKind::Std,
    NodeKind::Names,
    NodeKind::Bodies,
    NodeKind::Codegen,
    NodeKind::Store,
    NodeKind::Link,
    NodeKind::Test,
];

const STATES: [CrateState; 8] = [
    CrateState::Waiting,
    CrateState::Running,
    CrateState::UpToDate,
    CrateState::Built,
    CrateState::Failed,
    CrateState::Resolving,
    CrateState::Resolved,
    CrateState::Checking,
];

/// Names of crates and rows, some of the rows a node of a build script makes, one too long
/// for a narrow terminal and one of characters wider than a byte.
const NAMES: [&str; 9] = [
    "std",
    "libc",
    "winit",
    "wgpu",
    "anim",
    "regex_automata",
    "\u{fc}n\u{ef}c\u{f8}d\u{e9}",
    "a_crate_with_a_name_far_too_long_for_a_narrow_terminal_to_show_whole",
    "anyhow build script",
];

const OFS: [Option<&str>; 4] =
    [None, Some("anim"), Some("14 files"), Some("the standard library of build scripts")];

const FUNCTIONS: [&str; 4] =
    ["wgpu::Device::new", "wgpu::Queue::submit", "<regex_syntax::Folder>::new", "main"];

/// The same records for the same seed, on every machine.
struct Rng(SplitMix64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(SplitMix64::new(seed))
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.0.one_in(one_in)
    }

    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[self.0.index(of.len())]
    }
}

/// A build's records as a daemon could say them, from `rng`: steps begun and ended, nodes of
/// a graph said again and moved, crates, functions compiling, the cores, what was said, and
/// mostly an end. Times only go forward.
fn records(rng: &mut Rng, count: usize) -> Vec<Record> {
    let command = rng.pick(&["build", "run", "test"]).to_owned();
    let mut out = vec![at(0, Event::Began { build: 1, command, root: "/w".into() })];
    let mut open: Vec<(Step, Option<String>)> = Vec::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut now = 0;
    for _ in 0..count {
        now += rng.below(400) * MS;
        let id = |rng: &mut Rng, ids: &[u32]| match !ids.is_empty() && !rng.chance(5) {
            true => rng.pick(ids),
            false => rng.below(50) as u32,
        };
        let name = |rng: &mut Rng| rng.pick(&NAMES).to_owned();
        let slows = |rng: &mut Rng| -> Vec<Slow> {
            let slow = |rng: &mut Rng| Slow {
                krate: name(rng),
                name: rng.pick(&FUNCTIONS).to_owned(),
                ns: rng.below(2000) * MS,
            };
            (0..rng.below(5)).map(|_| slow(rng)).collect()
        };
        let event = match rng.below(22) {
            0..=2 => {
                let (step, of) = (rng.pick(&STEPS), rng.pick(&OFS).map(str::to_owned));
                open.push((step, of.clone()));
                Event::Started { step, of }
            }
            3 | 4 if !open.is_empty() && !rng.chance(8) => {
                let (step, of) = open.remove(rng.below(open.len() as u64) as usize);
                Event::Ended { step, of }
            }
            3 | 4 => Event::Ended { step: rng.pick(&STEPS), of: rng.pick(&OFS).map(str::to_owned) },
            5 => Event::Progress {
                step: match open.last() {
                    Some(it) if !rng.chance(4) => it.0,
                    _ => rng.pick(&STEPS),
                },
                done: rng.below(500),
                total: if rng.chance(3) { 0 } else { rng.below(1000) },
                longest: rng.chance(2).then(|| name(rng)),
            },
            6 | 7 => {
                let new = rng.below(40) as u32;
                let deps = (0..rng.below(4)).map(|_| id(rng, &ids)).collect();
                ids.push(new);
                Event::Node { id: new, kind: rng.pick(&KINDS), name: name(rng), deps }
            }
            8..=10 => {
                let state = match rng.below(8) {
                    0 => NodeState::Waiting,
                    1 => NodeState::Ready,
                    2 | 3 => NodeState::Running,
                    4 => NodeState::Built,
                    5 => NodeState::Cached { remote: rng.chance(2) },
                    6 => NodeState::Failed { why: "it did".into() },
                    _ if rng.chance(2) => NodeState::Skipped { because: 1 },
                    _ => NodeState::Cancelled,
                };
                Event::Moved { id: id(rng, &ids), state }
            }
            11 | 12 => {
                let total = if rng.chance(3) { 0 } else { rng.below(6000) };
                let fns = Fns { total, done: rng.below(6000), kept: rng.below(100) };
                Event::Crate { name: name(rng), state: rng.pick(&STATES), fns }
            }
            13 => {
                let cause = match rng.below(3) {
                    0 => Cause::Body,
                    1 => Cause::Signature,
                    _ => Cause::Callee { name: "render".into() },
                };
                Event::Rebuilt { name: rng.pick(&FUNCTIONS).to_owned(), krate: name(rng), cause }
            }
            14 => {
                let level = rng.pick(&[Level::Note, Level::Warning, Level::Error]);
                Event::Said { level, text: name(rng) }
            }
            15 => Event::Refused { name: name(rng), why: "it is a test".into() },
            16 => Event::Cores { busy: rng.below(1700) as u32, of: 1 + rng.below(16) as u32 },
            17 => Event::Compiling { functions: slows(rng) },
            18 => match rng.below(4) {
                0 => {
                    let cold = rng.chance(2).then(|| "the store is empty".to_owned());
                    let (found, fetched) = (rng.below(100), rng.below(3));
                    Event::Cache { cache: Cache { found, fetched, compiled: rng.below(9), cold } }
                }
                1 => Event::Critical { path: (0..rng.below(5)).map(|_| id(rng, &ids)).collect() },
                2 => Event::Unlisted { count: rng.below(10) },
                _ => Event::Slowest { functions: slows(rng) },
            },
            19 if rng.chance(30) => finished(rng, now),
            // Nodes said together, waiting for nodes said before and for each other, given
            // before or after them; now and then waiting in a cycle, or with an offset off.
            20 => {
                let (first, len) = (rng.below(40) as u32, rng.below(5) as u32);
                let (mut names, mut name_at) = (String::new(), vec![0]);
                let (mut deps, mut deps_at) = (Vec::new(), vec![0]);
                let kinds = (0..len).map(|_| rng.pick(&KINDS)).collect();
                for _ in 0..len {
                    names.push_str(&name(rng));
                    name_at.push(names.len() as u32);
                    for _ in 0..rng.below(3) {
                        deps.push(match rng.chance(3) {
                            true => first + rng.below(u64::from(len)) as u32,
                            false => id(rng, &ids),
                        });
                    }
                    deps_at.push(deps.len() as u32);
                }
                ids.extend(first..first + len);
                if len > 0 && rng.chance(10) {
                    name_at[len as usize] += 1;
                }
                Event::Nodes { first, kinds, names, name_at, deps, deps_at }
            }
            _ => Event::Functions { object: 0, whole: true, functions: Vec::new() },
        };
        out.push(at(now, event));
    }
    if !rng.chance(4) {
        out.push(at(now, finished(rng, now)));
    }
    out
}

fn finished(rng: &mut Rng, took_ns: u64) -> Event {
    let fns = Fns { total: 10, done: rng.below(11), kept: rng.below(3) };
    let rebuilt = match rng.chance(2) {
        true => vec![("render".to_owned(), Cause::Body)],
        false => Vec::new(),
    };
    let error = rng.chance(4).then(|| "it failed".to_owned());
    let summary = Summary { took_ns, crates: 1, fns, rebuilt, error, ..Summary::default() };
    Event::Finished { ok: rng.chance(4) || !rng.chance(3), summary }
}

/// What the fast build and the old have of the same records is the same, field by field.
fn same_build(new: &Build, was: &old::Build, now_ns: u64) {
    assert_eq!(new.command, was.command);
    assert_eq!(new.steps, was.steps);
    assert_eq!(new.crates, was.crates);
    let graph: Vec<_> = (0..new.graph.len())
        .map(|at| {
            let it = &new.graph[at];
            let deps: Vec<usize> = new.deps(at).iter().map(|&it| it as usize).collect();
            (it.id, it.kind, it.row as usize, deps)
        })
        .collect();
    let old_graph: Vec<_> =
        was.graph.iter().map(|it| (it.id, it.kind, it.row, it.deps.clone())).collect();
    assert_eq!(graph, old_graph);
    assert_eq!(new.cache, was.cache);
    assert_eq!(new.rebuilt, was.rebuilt);
    assert_eq!(new.unlisted, was.unlisted);
    assert_eq!(new.refused, was.refused);
    assert_eq!(new.finished, was.finished);
    assert_eq!(new.last_ns, was.last_ns);
    assert_eq!(new.unnamed, was.unnamed);
    assert_eq!(new.critical, was.critical);
    assert_eq!(new.cores, was.cores);
    assert_eq!(new.cores_of, was.cores_of);
    assert_eq!(new.compiling, was.compiling);
    assert_eq!(new.slowest, was.slowest);
    assert_eq!(new.idle, was.idle);
    assert_eq!(new.strip(now_ns), was.strip(now_ns));
    assert_eq!(new.fns(), was.fns());
    assert!(new.running().eq(was.running()));
    for name in NAMES {
        assert_eq!(new.compiling_of(name, now_ns), was.compiling_of(name, now_ns));
    }
}

/// Every index and count the build keeps is what a scan of it gives.
fn indexes_agree(build: &Build, now_ns: u64) {
    let index = &build.index;
    let crates = &build.crates;
    assert_eq!(index.rows.len(), crates.len());
    for (row, krate) in crates.iter().enumerate() {
        assert_eq!(crates.iter().position(|it| it.name == krate.name), Some(row));
        let hash = FixedState.hash_one(krate.name.as_str());
        let found = index.rows.find(hash, |&it| crates[it as usize].name == krate.name);
        assert_eq!(found, Some(&(row as u32)));
    }
    for node in &build.graph {
        let scan = build.graph.iter().rposition(|it| it.id == node.id);
        assert_eq!(index.nodes.get(&node.id).map(|&it| it as usize), scan);
    }
    let open: Vec<u32> = (0..build.steps.len() as u32)
        .filter(|&it| build.steps[it as usize].ended_ns.is_none())
        .collect();
    assert_eq!(index.open, open);
    let unknown = crates.iter().filter(|it| crate::unknown(it.fns)).count();
    assert_eq!(index.unknown as usize, unknown);
    for state in STATES {
        assert_eq!(build.in_state(state), crates.iter().filter(|it| it.state == state).count());
    }
    assert_eq!(index.ended as usize, crates.iter().filter(|it| it.ended_ns.is_some()).count());
    let graph = &build.graph;
    let busy = |node: usize| crates[graph[node].row as usize].is_busy();
    let (mut under_way, mut waited_for) = (vec![false; graph.len()], vec![false; graph.len()]);
    for at in 0..graph.len() {
        under_way[at] = busy(at) || build.deps(at).iter().any(|&dep| under_way[dep as usize]);
        for &dep in build.deps(at) {
            waited_for[dep as usize] = true;
        }
    }
    assert_eq!(index.under_way, under_way);
    assert_eq!(index.waited_for, waited_for);
    let mut rows: Vec<u32> = graph.iter().map(|it| it.row).collect();
    rows.sort_unstable();
    rows.dedup();
    let busy_rows = rows.iter().filter(|&&it| crates[it as usize].is_busy()).count();
    assert_eq!(index.busy_graphed as usize, busy_rows);
    for name in NAMES {
        let (top, len) = build.compiling_top::<3>(name, now_ns);
        let top: Vec<(&str, u64)> =
            top[..len].iter().map(|&(at, ns)| (build.compiling[at].0.name.as_str(), ns)).collect();
        let scan: Vec<(&str, u64)> = build.compiling_of(name, now_ns).into_iter().take(3).collect();
        assert_eq!(top, scan);
    }
}

/// The region diffed against what is on the screen leaves the screen as clearing it and
/// drawing it whole did, record after record of seeded builds, ticks and resizes between
/// them, at every width and in color; with the cursor in the same place, never more bytes,
/// and the build and its indexes as the old scans have them.
#[test]
fn the_diffed_region_shows_what_redrawing_it_whole_did() {
    let (mut fast_bytes, mut whole_bytes) = (0, 0);
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let records = records(&mut rng, 120);
        let width = 20 + rng.below(130) as usize;
        let color = rng.chance(2);
        let (mut new, mut was) = (every_record(width, color), old::Live::new(width, color));
        if rng.chance(4) {
            (new, was) = (new.errors_said(), was.errors_said());
        }
        let (mut fast, mut whole) = (Screen::default(), Screen::default());
        let mut now = 0;
        let mut draw = |a: String, b: String, at: &str| {
            assert!(a.len() <= b.len(), "seed {seed}, {at}: {a:?} for {b:?}");
            (fast_bytes, whole_bytes) = (fast_bytes + a.len(), whole_bytes + b.len());
            fast.feed(&a);
            whole.feed(&b);
            assert_eq!(fast.lines(), whole.lines(), "seed {seed}, {at}: {a:?} for {b:?}");
            assert_eq!((fast.row, fast.col), (whole.row, whole.col), "seed {seed}, {at}");
        };
        for (at, record) in records.iter().enumerate() {
            now = now.max(record.at_ns);
            for _ in 0..rng.below(3) {
                now += rng.below(300) * MS;
                draw(new.tick(now), was.tick(now), &format!("a tick before record {at}"));
            }
            if rng.chance(25) {
                let width = 20 + rng.below(130) as usize;
                new.resize(width);
                was.resize(width);
            }
            draw(new.record(record, now), was.record(record, now), &format!("record {at}"));
            same_build(new.build(), &was.build, now);
            indexes_agree(new.build(), now);
            if rng.chance(10) {
                assert_eq!(new.snapshot(now), was.snapshot(now), "seed {seed}, record {at}");
            }
        }
        draw(new.finish(), was.finish(), "the finish");
    }
    assert!(fast_bytes < whole_bytes, "{fast_bytes} bytes for {whole_bytes}");
}

/// Records that come together, drawn once, leave the screen as each drawn alone did.
#[test]
fn records_come_together_show_what_each_alone_did() {
    for seed in 0..200 {
        let mut rng = Rng::new(1000 + seed);
        let records = records(&mut rng, 120);
        let (width, color) = (20 + rng.below(130) as usize, rng.chance(2));
        let (mut each, mut together) = (every_record(width, color), every_record(width, color));
        let (mut alone, mut batched) = (Screen::default(), Screen::default());
        let mut rest = records.as_slice();
        while !rest.is_empty() {
            let take = (1 + rng.below(8) as usize).min(rest.len());
            let (chunk, after) = rest.split_at(take);
            let now = chunk.last().map_or(0, |it| it.at_ns) + rng.below(100) * MS;
            for record in chunk {
                alone.feed(&each.record(record, now));
            }
            let mut out = String::new();
            together.records_into(chunk, now, &mut out);
            batched.feed(&out);
            assert_eq!(alone.lines(), batched.lines(), "seed {seed}");
            assert_eq!((alone.row, alone.col), (batched.row, batched.col), "seed {seed}");
            rest = after;
        }
    }
}

/// Records far more often than a screen shows frames are written at most once a frame, and
/// once a frame has passed the screen shows what drawing after every record showed.
#[test]
fn records_faster_than_frames_are_drawn_once_a_frame() {
    for seed in 0..200 {
        let mut rng = Rng::new(9000 + seed);
        let records = records(&mut rng, 120);
        let (width, color) = (20 + rng.below(130) as usize, rng.chance(2));
        let (mut capped, mut each) = (Live::new(width, color), every_record(width, color));
        let (mut held, mut alone) = (Screen::default(), Screen::default());
        let (mut now, mut writes, mut last_write) = (0, 0u64, None::<u64>);
        for (at, record) in records.iter().enumerate() {
            now = now.max(record.at_ns) + rng.below(4) * MS;
            let mut out = String::new();
            capped.record_into(record, now, &mut out);
            alone.feed(&each.record(record, now));
            if !out.is_empty() {
                let finished = capped.build().finished.is_some();
                if let Some(last) = last_write {
                    assert!(finished || now - last >= crate::FRAME_NS, "seed {seed}, record {at}: drawn {}ns after the last", now - last);
                }
                writes += 1;
                last_write = Some(now);
                held.feed(&out);
            }
            if capped.due_ns().is_none() {
                assert_eq!(held.lines(), alone.lines(), "seed {seed}, record {at}: drawn, the screens agree");
            }
        }
        if let Some(due) = capped.due_ns() {
            now = now.max(due);
            held.feed(&capped.tick(now));
            alone.feed(&each.tick(now));
        }
        assert_eq!(capped.due_ns(), None, "seed {seed}");
        assert_eq!(held.lines(), alone.lines(), "seed {seed}: once due");
        held.feed(&capped.finish());
        alone.feed(&each.finish());
        assert_eq!(held.lines(), alone.lines(), "seed {seed}: finished");
        assert!(writes <= 1 + now / crate::FRAME_NS + 1, "seed {seed}: {writes} writes in {now}ns");
    }
}

/// The lines off a terminal are the old ones, byte for byte.
#[test]
fn plain_lines_are_the_old_ones() {
    for seed in 0..300 {
        let mut rng = Rng::new(5000 + seed);
        let records = records(&mut rng, 120);
        let color = rng.chance(2);
        let (mut new, mut was) = (Plain::new(color), old::Plain::new(color));
        if rng.chance(4) {
            (new, was) = (new.errors_said(), was.errors_said());
        }
        let mut now = 0;
        for (at, record) in records.iter().enumerate() {
            now = now.max(record.at_ns);
            if rng.chance(3) {
                now += rng.below(20) * S;
                assert_eq!(new.tick(now), was.tick(now), "seed {seed}, a tick before {at}");
            }
            assert_eq!(new.record(record), was.record(record), "seed {seed}, record {at}");
            same_build(new.build(), &was.build, now);
        }
    }
}

/// A build's end from `rng`, each of its parts there or not, so that every kind of last line
/// is said: what it rebuilt or compiled, the store, its slowest steps with their cores, its
/// slowest functions with long names, the chain, time unnamed, a failure.
fn finished_whole(rng: &mut Rng) -> Finished {
    let causes = [Cause::Body, Cause::Signature, Cause::Callee { name: "render".into() }];
    let rebuilt = (0..rng.below(4))
        .map(|_| (rng.pick(&FUNCTIONS).to_owned(), causes[rng.below(3) as usize].clone()))
        .collect();
    let cache = rng.chance(2).then(|| Cache {
        found: rng.below(100),
        fetched: rng.below(3),
        compiled: rng.below(9),
        cold: rng.chance(2).then(|| "the store is empty".to_owned()),
    });
    let took = |rng: &mut Rng| Took {
        step: rng.pick(&STEPS),
        of: rng.pick(&OFS).map(str::to_owned),
        took_ns: rng.below(200) * S / 3,
    };
    let summary = Summary {
        took_ns: if rng.chance(4) { 0 } else { rng.below(300) * S / 7 },
        crates: rng.below(3),
        fns: Fns { total: 10, done: rng.below(11), kept: rng.below(3) },
        rebuilt,
        more: rng.below(3),
        error: rng.chance(4).then(|| "it failed".to_owned()),
        cache,
        slowest: (0..rng.below(4)).map(|_| took(rng)).collect(),
    };
    let long = |rng: &mut Rng| "x::\u{e9}".repeat(rng.below(20) as usize) + rng.pick(&FUNCTIONS);
    let slow = |rng: &mut Rng| {
        let krate = rng.pick(&NAMES).to_owned();
        Slow { krate, name: long(rng), ns: rng.below(2000) * MS }
    };
    let busy = |rng: &mut Rng| rng.chance(2).then(|| rng.below(1600) as f64 / 100.0);
    let cores = summary.slowest.iter().map(|_| busy(rng)).collect();
    let unnamed = |rng: &mut Rng| Unnamed { took_ns: rng.below(4) * S / 2, after: rng.pick(&STEPS) };
    Finished {
        ok: !rng.chance(4),
        summary,
        at_ns: rng.below(100) * S / 9,
        chain: (0..rng.below(4)).map(|_| rng.pick(&NAMES).to_owned()).collect(),
        chain_ns: rng.below(3) * S,
        unnamed: rng.chance(2).then(|| unnamed(rng)),
        cores,
        cores_of: 1 + rng.below(16) as u32,
        slowest: (0..rng.below(8)).map(|_| slow(rng)).collect(),
    }
}

/// The last lines and `blaze why`'s steps, written in place, are the ones joined from a
/// `String` per piece, byte for byte, in color and not.
#[test]
fn the_last_lines_written_in_place_are_the_old_ones() {
    for seed in 0..2000 {
        let mut rng = Rng::new(7000 + seed);
        let finished = finished_whole(&mut rng);
        for color in [false, true] {
            let (now, was) = (summary_line(&finished, color), old::summary_line(&finished, color));
            assert_eq!(now, was, "seed {seed}");
            assert_eq!(why_lines(&finished, color), old::why_lines(&finished, color), "seed {seed}");
        }
        let text = "x::\u{e9}".repeat(rng.below(30) as usize);
        let most = rng.below(60) as usize;
        assert_eq!(crate::tail(&text, most), old::tail(&text, most), "seed {seed}");
    }
    for seed in 0..300 {
        let mut rng = Rng::new(8000 + seed);
        let (mut build, mut now) = (Build::default(), 0);
        for record in records(&mut rng, 120) {
            now = now.max(record.at_ns);
            build.apply(&record);
        }
        let now = now + rng.below(5) * S;
        assert_eq!(build.why_steps(now), old::why_steps(&build, now), "seed {seed}");
    }
}

/// A record that moves what the region shows, warm (once the rows it names exist and every
/// buffer has held a draw as long): applying it to the build, drawing the region, diffing it
/// with the one shown and writing what changed allocate nothing. Neither does a tick. With a
/// graph, the tree and its longest chain are drawn too.
#[test]
fn a_warm_record_to_the_terminal_allocates_nothing() {
    let t = 5 * S;
    let slow = |name: &str| Slow { krate: "anim".into(), name: name.into(), ns: 3 * S };
    let progress = |done, longest: &str| Event::Progress {
        step: Step::Codegen,
        done,
        total: 9,
        longest: Some(longest.into()),
    };
    // It ends as it began, so each round draws what the one before it did.
    let round = [
        at(t, anim(CrateState::Running, 1000, 10)),
        at(t, progress(3, "anim::scene::render")),
        at(t, Event::Moved { id: 2, state: NodeState::Running }),
        at(t, Event::Compiling { functions: vec![slow("anim::a"), slow("anim::b")] }),
        at(t, Event::Said { level: Level::Warning, text: "unused variable".into() }),
        at(t, anim(CrateState::Running, 2000, 20)),
        at(t, Event::Compiling { functions: vec![slow("anim::c")] }),
        at(t, Event::Moved { id: 2, state: NodeState::Built }),
        at(t, progress(4, "anim::scene::draw")),
        at(t, anim(CrateState::Running, 1000, 10)),
    ];
    for graph in [false, true] {
        let mut setup = vec![
            at(0, Event::Began { build: 1, command: "build".into(), root: "/w/anim".into() }),
            at(MS, Event::Started { step: Step::Codegen, of: None }),
            at(MS, Event::Cores { busy: 300, of: 8 }),
        ];
        if graph {
            let node = |id, name: &str, deps: &[u32]| {
                let (kind, name, deps) = (NodeKind::Codegen, name.into(), deps.to_vec());
                at(MS, Event::Node { id, kind, name, deps })
            };
            setup.extend([node(1, "syn", &[]), node(2, "anim", &[1])]);
            setup.push(at(MS, Event::Critical { path: vec![1, 2] }));
        }
        let mut live = every_record(100, true);
        let mut out = String::new();
        live.records_into(&setup, t, &mut out);
        let play = |live: &mut Live, out: &mut String| {
            for record in &round {
                out.clear();
                live.record_into(record, t, out);
            }
            out.clear();
            live.tick_into(t, out);
        };
        play(&mut live, &mut out);
        play(&mut live, &mut out);
        blaze_alloc_count::assert_at_most(0, "a warm record to the terminal, drawn and diffed", || {
            play(&mut live, &mut out);
        });
        assert!(live.drawn() > 0, "graph {graph}: the region is drawn");
    }
}

/// The daemon's memory, one line a tag and the committed total, with no budget said as none.
#[test]
fn memory_is_a_line_a_tag_and_the_committed_total() {
    use blaze_api::{Memory, Response, TagMemory};
    let tag = |tag: &str, budget: u64| TagMemory {
        tag: tag.to_owned(),
        live: 4096,
        rate: 10,
        budget,
        allocated: 8192,
        task_peak: 512,
        high_water: 6000,
    };
    let memory = Memory {
        tags: vec![tag("parse", u64::MAX), tag("link", 1 << 20)],
        committed: 1 << 21,
        committed_high_water: 3 << 20,
    };
    assert_eq!(
        crate::api::lines(&Response::Memory(memory), false),
        "parse: 4096 bytes live, 6000 at most, 10 bytes/s, task peak 512, budget none\n\
         link: 4096 bytes live, 6000 at most, 10 bytes/s, task peak 512, budget 1048576\n\
         committed: 2097152 bytes, 3145728 at most\n"
    );
}
