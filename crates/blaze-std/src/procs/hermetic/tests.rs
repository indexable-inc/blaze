//! The proof of the hermetic run, in the simulation (rule 64): a drawn script reads inside and
//! outside its roots, writes inside and outside its output folder, prints, burns, sleeps past
//! its deadline, passes its budget or crashes; the shipped path ([`run_through`]) runs it on
//! the simulated OS, twice, from two start times. The oracle, in the order it is checked:
//!
//! 1. the child's environment is exactly the spec's pairs (nothing inherited, no clock leaked);
//! 2. nothing from outside the roots (or an undeclared file of a package root) is in any
//!    output, and nothing was written outside the output folder;
//! 3. a hang is stopped by the deadline, a budget's pass by the budget, and a stop or a crash
//!    leaves the output folder empty;
//! 4. a run that reported a read outside its roots is refused;
//! 5. an accepted run's read list holds every read the script made (its reported reads, or
//!    every admitted package file when there was no report);
//! 6. the two runs are equal.
//!
//! Each planted bug ([`HermeticPlant`]) is caught by its invariant (`controls`).

use core::{cell::RefCell, fmt, time::Duration};
use std::{string::String, vec::Vec as StdVec};

use blaze_core::wait::Deadline;
use blaze_own::Alloc;
use blaze_roots::{Digest, Glob, Mount, Root, RootId, RootKind, Roots, Seen};
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use super::{
    Act, Budget, Capture, ClockSpec, Fault, HermeticOpts, HermeticPlant, HermeticSpec, HermeticStreams, Level, LinesReport,
    Program, Reads, ReportSpec, SimFiles, SimOs, SimPort, Source, Stop, Stream, run_through,
};
use crate::procs::Exit;

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const SECRET: &[u8] = b"SECRET-OUTSIDE-THE-ROOTS";
const UNDECLARED: &[u8] = b"UNDECLARED-IN-THE-PACKAGE";

const PROGRAM: &str = "/t/bin/cc";
const TREE_FILE: &str = "/t/include/a.h";
const PKG_LIB: &str = "/w/pkg/src/lib.c";
const PKG_HEADER: &str = "/w/pkg/src/x.h";
const PKG_UNDECLARED: &str = "/w/pkg/notes.txt";
const SYSTEM_LIB: &str = "/usr/lib/libc.so";
const OUTSIDE: &str = "/home/u/key";
const SYSTEM: [&str; 1] = ["/usr/lib"];

#[derive(Clone, PartialEq, Eq, Debug)]
enum HermeticBroken {
    StartedUnconfined,
    EnvNotExact { got: String },
    OutsideInOutput { what: &'static str },
    HangNotStopped,
    BudgetNotStopped,
    StopKeptOutput,
    OutsideAccepted { path: String },
    ReadMissing { path: String },
    Unequal,
}

impl fmt::Display for HermeticBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HermeticBroken::StartedUnconfined => f.write_str("a hermetic run started its child without a confinement"),
            HermeticBroken::EnvNotExact { got } => write!(f, "the child's environment was {got:?}, not exactly the spec's"),
            HermeticBroken::OutsideInOutput { what } => write!(f, "{what}"),
            HermeticBroken::HangNotStopped => f.write_str("a child that outlived its deadline was not stopped by it"),
            HermeticBroken::BudgetNotStopped => f.write_str("a child past its budget was not stopped by it"),
            HermeticBroken::StopKeptOutput => f.write_str("a stopped or crashed run left files in its output folder"),
            HermeticBroken::OutsideAccepted { path } => write!(f, "a run that reported reading {path}, outside its roots, was accepted"),
            HermeticBroken::ReadMissing { path } => write!(f, "{path} was read and is not in the accepted read list"),
            HermeticBroken::Unequal => f.write_str("two runs of one spec differ"),
        }
    }
}

blaze_sim::plants! {
    impl HermeticPlant breaks HermeticBroken {
        None,
        HoleInPlan => HermeticBroken::OutsideInOutput { .. },
        ReportUnchecked => HermeticBroken::OutsideAccepted { .. },
        EnvInherited => HermeticBroken::EnvNotExact { .. },
        ClockLeaks => HermeticBroken::EnvNotExact { .. },
        PartialKept => HermeticBroken::StopKeptOutput,
        ReadDropped => HermeticBroken::ReadMissing { .. },
        ConfineDropped => HermeticBroken::StartedUnconfined,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    Exit(i32),
    Crash,
    Hang,
    BurnCpu,
    BurnMemory,
}

struct Case {
    script: StdVec<Act<'static>>,
    end: End,
    fixed_clock: bool,
    report: bool,
}

fn draw(tape: &Tape) -> Case {
    let palette: [Act<'static>; 14] = [
        Act::Read { path: TREE_FILE },
        Act::Read { path: PKG_LIB },
        Act::Read { path: PKG_UNDECLARED },
        Act::Read { path: OUTSIDE },
        Act::Read { path: SYSTEM_LIB },
        Act::Load { path: SYSTEM_LIB },
        Act::Write { path: "/o/out.o", from: Source::Seen },
        Act::Write { path: "/o/env", from: Source::Env },
        Act::Write { path: "/w/pkg/src/evil.c", from: Source::Bytes(b"evil") },
        Act::Write { path: "/tmp/x", from: Source::Bytes(b"x") },
        Act::Print { stream: Stream::Stdout, bytes: b"hello" },
        Act::Print { stream: Stream::Stderr, bytes: b"warn" },
        Act::Sleep { time: Duration::from_millis(5) },
        Act::Burn { cpu: Duration::from_millis(50), memory: 1 << 20 },
    ];
    let mut script = tape.vec(0..=10, "acts", |t| palette[t.index(palette.len(), "act")]);
    let end = match tape.int_in(0u8..=6, "end") {
        0 | 1 => End::Exit(0),
        2 => End::Exit(1),
        3 => End::Crash,
        4 => End::Hang,
        5 => End::BurnCpu,
        _ => End::BurnMemory,
    };
    match end {
        End::Exit(status) => script.push(Act::Exit { status }),
        End::Crash => script.push(Act::Crash { signal: 11 }),
        End::Hang => script.push(Act::Sleep { time: Duration::from_secs(100) }),
        End::BurnCpu => {
            script.push(Act::Burn { cpu: Duration::from_millis(700), memory: 0 });
            script.push(Act::Exit { status: 0 });
        }
        End::BurnMemory => {
            script.push(Act::Burn { cpu: Duration::from_millis(30), memory: 2 << 30 });
            script.push(Act::Exit { status: 0 });
        }
    }
    Case { script, end, fixed_clock: tape.coin("fixed clock"), report: tape.coin("report") }
}

/// What one run came to, in the test's own values.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Outcome {
    result: Result<Accepted, Fault>,
    started: bool,
    confined: Option<u32>,
    env: String,
    reported: StdVec<String>,
    files: StdVec<(String, StdVec<u8>)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Accepted {
    exit: Exit,
    outputs: Digest,
    stdout: StdVec<u8>,
    stderr: StdVec<u8>,
    exact: bool,
    reads: StdVec<(u8, String, Seen)>,
    reads_digest: Digest,
}

fn one_run(case: &Case, start: Duration, plant: impl Planted<HermeticPlant>) -> Result<Outcome, Broken<HermeticBroken>> {
    let alloc = heap();
    let os = RefCell::new(SimOs::new(alloc));
    {
        let mut os = os.borrow_mut();
        let files: [(&str, &[u8]); 7] = [
            (PROGRAM, b"cc"),
            (TREE_FILE, b"tree-header"),
            (PKG_LIB, b"lib"),
            (PKG_HEADER, b"header"),
            (PKG_UNDECLARED, UNDECLARED),
            (SYSTEM_LIB, b"libc"),
            (OUTSIDE, SECRET),
        ];
        for (path, bytes) in files {
            os.put(path.as_bytes(), bytes).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
        }
    }
    let globs = [Glob::new("src/**").map_err(|e| Broken::Harness(std::format!("{e:?}")))?];
    let list = [
        Root { name: "toolchain", kind: RootKind::Tree { tree: Digest([3; 32]) } },
        Root { name: "package", kind: RootKind::Package { globs: &globs } },
        Root { name: "out", kind: RootKind::Output },
    ];
    let roots = Roots::new(&list).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
    let mounts = [
        Mount { spelling: b"/t", root: RootId(0) },
        Mount { spelling: b"/w/pkg", root: RootId(1) },
        Mount { spelling: b"/o", root: RootId(2) },
    ];
    let fixed: [(&str, &str); 2] = [("LANG", "C"), ("SOURCE_DATE_EPOCH", "1700000000")];
    let (env, clock): (&[(&str, &str)], ClockSpec) =
        if case.fixed_clock { (&fixed, ClockSpec::Fixed { unix_secs: 1_700_000_000 }) } else { (&[], ClockSpec::NeverAsks) };
    let parser = LinesReport;
    let deps = blaze_roots::RelPath::new("deps", 64).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
    let report = if case.report { ReportSpec::Program { file: deps, parser: &parser } } else { ReportSpec::WholeRoots };
    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: "cc", digest: Digest([9; 32]), path: PROGRAM },
        args: &["-c", "src/lib.c"],
        env,
        roots,
        mounts: &mounts,
        cwd: "/w/pkg",
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock,
        seed: 7,
        budget: Budget { cpu: Duration::from_millis(500), memory: 1 << 30, steps: None },
        deadline: Deadline::at(Duration::from_secs(3)),
        streams: HermeticStreams { stdout: Capture::Keep { most: 64 }, stderr: Capture::Keep { most: 64 } },
        level: Level::Inputs,
        report,
    })
    .map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
    let files = SimFiles { os: &os, spec: &spec, alloc };
    let mut port = SimPort::new(&os, &case.script, start, alloc);
    let result = run_through(&spec, &mut port, &files, &SYSTEM, alloc, plant);
    let result = match result {
        Ok(run) => Ok(Accepted {
            exit: run.exit,
            outputs: run.outputs.digest(),
            stdout: run.stdout.as_slice().to_vec(),
            stderr: run.stderr.as_slice().to_vec(),
            exact: matches!(run.reads, Reads::Exact(_)),
            reads: run.reads.checked().entries().map(|e| (e.root.0, String::from(e.path.as_str()), e.seen)).collect(),
            reads_digest: run.reads.checked().digest(),
        }),
        Err(error) => Err(error.fault),
    };
    let env = String::from_utf8_lossy(port.env_given()).into_owned();
    let reported = port.reported().split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(|l| String::from_utf8_lossy(l).into_owned()).collect();
    let files = os.borrow().under(b"").map(|(p, c)| (String::from_utf8_lossy(p).into_owned(), c.to_vec())).collect();
    Ok(Outcome { result, started: port.started(), confined: port.confined(), env, reported, files })
}

fn root_of(path: &str) -> Option<(u8, &str)> {
    for (spelling, root) in [("/t/", 0u8), ("/w/pkg/", 1), ("/o/", 2)] {
        if let Some(rest) = path.strip_prefix(spelling) {
            return Some((root, rest));
        }
    }
    None
}

fn oracle(case: &Case, outcome: &Outcome) -> Result<(), Broken<HermeticBroken>> {
    // 0. No child starts without its confinement (the start's confine step, recorded by the
    //    simulated start).
    if outcome.started && outcome.confined.is_none() {
        return Err(Broken::Oracle(HermeticBroken::StartedUnconfined));
    }
    // 1. Exactly the spec's environment.
    let expected = if case.fixed_clock { "LANG=C\nSOURCE_DATE_EPOCH=1700000000\n" } else { "" };
    if outcome.started && outcome.env != expected {
        return Err(Broken::Oracle(HermeticBroken::EnvNotExact { got: outcome.env.clone() }));
    }
    // 2. Nothing from outside in an output; nothing written outside the output folder.
    for (path, content) in &outcome.files {
        let under_out = path.starts_with("o/");
        if under_out && content.windows(SECRET.len()).any(|w| w == SECRET) {
            return Err(Broken::Oracle(HermeticBroken::OutsideInOutput { what: "an output holds a file from outside the roots" }));
        }
        if under_out && content.windows(UNDECLARED.len()).any(|w| w == UNDECLARED) {
            return Err(Broken::Oracle(HermeticBroken::OutsideInOutput { what: "an output holds a package file its globs do not declare" }));
        }
        if path == "w/pkg/src/evil.c" || path == "tmp/x" {
            return Err(Broken::Oracle(HermeticBroken::OutsideInOutput { what: "the child wrote outside its output folder" }));
        }
    }
    // 3. Stops, and what they leave.
    let stopped = matches!(outcome.result, Err(Fault::Stopped(_)));
    let crashed = matches!(outcome.result, Ok(Accepted { exit: Exit::Signaled(_), .. }));
    if (stopped || crashed) && outcome.files.iter().any(|(path, _)| path.starts_with("o/")) {
        return Err(Broken::Oracle(HermeticBroken::StopKeptOutput));
    }
    // A stream's cap or a budget may stop a hanging child first: any stop is its end.
    if case.end == End::Hang && !stopped {
        return Err(Broken::Oracle(HermeticBroken::HangNotStopped));
    }
    let budget_or_stream = matches!(outcome.result, Err(Fault::Stopped(Stop::Budget(_) | Stop::Stream { .. })));
    if matches!(case.end, End::BurnCpu | End::BurnMemory) && !budget_or_stream {
        return Err(Broken::Oracle(HermeticBroken::BudgetNotStopped));
    }
    let Ok(accepted) = &outcome.result else { return Ok(()) };
    // 4. A reported read outside the roots refuses the run.
    if accepted.exact {
        if let Some(outside) = outcome.reported.iter().find(|path| root_of(path).is_none()) {
            return Err(Broken::Oracle(HermeticBroken::OutsideAccepted { path: outside.clone() }));
        }
    }
    // 5. Every read is in the list.
    let wanted: StdVec<(u8, String)> = if accepted.exact {
        outcome.reported.iter().filter_map(|path| root_of(path).map(|(root, rest)| (root, String::from(rest)))).collect()
    } else {
        [(1u8, String::from("src/lib.c")), (1, String::from("src/x.h"))].into()
    };
    for (root, path) in wanted {
        if !accepted.reads.iter().any(|(r, p, _)| *r == root && *p == path) {
            return Err(Broken::Oracle(HermeticBroken::ReadMissing { path }));
        }
    }
    Ok(())
}

fn hermetic(tape: &Tape, plant: impl Planted<HermeticPlant>) -> Result<(), Broken<HermeticBroken>> {
    let case = draw(tape);
    let first = one_run(&case, Duration::ZERO, plant)?;
    oracle(&case, &first)?;
    let second = one_run(&case, Duration::from_millis(1500), plant)?;
    oracle(&case, &second)?;
    // 6. One spec, one result.
    if first != second {
        return Err(Broken::Oracle(HermeticBroken::Unequal));
    }
    Ok(())
}

#[test]
fn a_hermetic_run_keeps_inside_its_roots_and_is_equal_twice() {
    check(Config::cases(1000), |tape| hermetic(tape, NoPlant));
}

#[test]
fn every_hermetic_plant_is_caught_by_its_invariant() {
    controls(Config::cases(2000), |tape, plant: HermeticPlant| hermetic(tape, plant));
}

#[test]
fn execution_is_refused_naming_the_level_and_the_backend() {
    let list = [Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = [Mount { spelling: b"/o", root: RootId(0) }];
    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Records { name: "build-script", code: Digest([1; 32]) },
        args: &[],
        env: &[],
        roots,
        mounts: &mounts,
        cwd: "/o",
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock: ClockSpec::NeverAsks,
        seed: 0,
        budget: Budget { cpu: Duration::from_secs(1), memory: 1 << 30, steps: Some(1 << 20) },
        deadline: Deadline::at(Duration::from_secs(1)),
        streams: HermeticStreams { stdout: Capture::Null, stderr: Capture::Null },
        level: Level::Execution,
        report: ReportSpec::WholeRoots,
    })
    .unwrap();
    let backend = super::EXECUTION_BACKENDS[0];
    let why = backend.absent().unwrap();
    assert_eq!(why, super::Absent::NotBuilt { piece: "crates/blaze-comptime: the host-call system layer D175 runs code records against" });
    assert_eq!(spec.level(), Level::Execution);
    assert_eq!(super::EXECUTION_BACKENDS[1].absent(), Some(super::Absent::OwnerHasNotAllowed));
}

#[test]
fn a_spec_has_one_spelling() {
    let list = [Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = [Mount { spelling: b"/o", root: RootId(0) }];
    let opts = |env: &'static [(&'static str, &'static str)], clock, steps| HermeticOpts {
        program: Program::Stored { name: "cc", digest: Digest([9; 32]), path: "/o/cc" },
        args: &[],
        env,
        roots,
        mounts: &mounts,
        cwd: "/o",
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock,
        seed: 0,
        budget: Budget { cpu: Duration::from_secs(1), memory: 1 << 30, steps },
        deadline: Deadline::at(Duration::from_secs(1)),
        streams: HermeticStreams { stdout: Capture::Null, stderr: Capture::Null },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    };
    let fault = |opts| HermeticSpec::new(opts).err().map(|e| e.fault);
    assert_eq!(fault(opts(&[("B", "1"), ("A", "2")], ClockSpec::NeverAsks, None)), Some(Fault::EnvNotSorted { at: 1 }));
    assert_eq!(fault(opts(&[("A=B", "1")], ClockSpec::NeverAsks, None)), Some(Fault::EnvSpelling { at: 0 }));
    assert_eq!(fault(opts(&[], ClockSpec::Fixed { unix_secs: 5 }, None)), Some(Fault::ClockNotInEnv));
    assert_eq!(fault(opts(&[("SOURCE_DATE_EPOCH", "6")], ClockSpec::Fixed { unix_secs: 5 }, None)), Some(Fault::ClockNotInEnv));
    assert_eq!(fault(opts(&[], ClockSpec::NeverAsks, Some(1))), Some(Fault::StepsNotMetered));
    assert_eq!(fault(opts(&[("SOURCE_DATE_EPOCH", "5")], ClockSpec::Fixed { unix_secs: 5 }, None)), None);
}
