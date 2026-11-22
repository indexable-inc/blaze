//! THE SIMULATION: a model guest drives the [`Guest`] and [`Host`] machines over the real wire
//! against a model world (a tree root, a package root with globs and files it does not
//! declare, an output root), and the oracle, written apart from the host from the world's
//! rules, checks every answer and every end:
//!
//! - nothing outside the grants is ever answered with content (a lookup's answer is exactly the
//!   model's, a variable's value is its grant's);
//! - the read list holds exactly what was answered (each admitted lookup, with what it saw);
//! - the clock is the grants' fixed clock; the random bytes are the seed's stream;
//! - the same case run twice at two host times gives equal answers, findings, output, read
//!   list and facts;
//! - fuel and deadline stops, crashes and the ordinary end are the ones the case calls for, and
//!   only a finished run has output (`Ended` holds none: by its type);
//! - every refusal is a finding naming the path or name and the grant that would allow it.
//!
//! Controls plant the three bugs of plant.rs: an undeclared read answered, a read left off the
//! list, the host's clock leaked.

use core::fmt;
use std::{format, string::String, vec, vec::Vec as StdVec};

use blaze_own::Alloc;
use blaze_roots::{Digest, Glob, Mount, Mounts, Root, RootId, RootKind, Roots, Seen, listing_digest};
use blaze_sim::{Broken, Config, Planted, Tape, check, controls};

use crate::wire::split_frame;
use crate::{
    Answer, BuildScriptOpts, Call, CallKey, ClockKind, Deadline, Done, Effect, Ended, EnvGrant, EnvValue, Event,
    FileKind, Found, Fuel, Grant, Grants, Guest, GuestEffect, GuestEvent, GuestOpts, Host, HostOpts, HostPlant, Nanos,
    ProcMacroOpts, Seed, Stream, SysError, Want, Words,
};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const CPU_FUEL: u64 = 1_000_000;
const DEADLINE: u64 = 1_000_000_000;

// The world: what is really on disk, declared or not.
const TREE: [(&str, &[u8]); 2] = [("a.txt", b"tree a"), ("dir/b.txt", b"tree b")];
const PKG: [(&str, &[u8]); 5] = [
    ("Cargo.toml", b"[package]"),
    ("src/lib.rs", b"fn f() {}"),
    ("abi/x.json", b"{}"),
    ("abi/x.txt", b"undeclared"),
    ("secret.key", b"hunter2"),
];

const PATHS: [&str; 20] = [
    "/pkg/src/lib.rs",
    "src/lib.rs",
    "Cargo.toml",
    "abi/x.json",
    "abi/x.txt",
    "secret.key",
    "abi",
    "src",
    "/pkg",
    ".",
    "/tree/a.txt",
    "/tree/dir",
    "/tree",
    "../tree/dir/b.txt",
    "/etc/passwd",
    "../../x",
    "nothere.rs",
    "src/nothere.rs",
    "/out/gen.rs",
    "/out",
];
const NAMES: [&str; 4] = ["CARGO_PKG_NAME", "HOME", "OUT_DIR", "SECRET_TOKEN"];
const OUT_PATHS: [&str; 4] = ["/out/gen.rs", "/out/sub/x.rs", "/pkg/src/lib.rs", "/out/sub"];

/// The package's declared files and directories, by the plain rules of its globs `Cargo.toml`,
/// `src/**`, `abi/*.json` (the tree root admits everything).
fn model_file(root: u8, rel: &str) -> bool {
    root == 0
        || rel == "Cargo.toml"
        || rel.starts_with("src/")
        || (rel.starts_with("abi/") && !rel["abi/".len()..].contains('/') && rel.ends_with(".json"))
}

fn model_dir(root: u8, rel: &str) -> bool {
    root == 0 || rel.is_empty() || rel == "src" || rel.starts_with("src/") || rel == "abi"
}

fn admitted(root: u8, rel: &str) -> bool {
    model_file(root, rel) || model_dir(root, rel)
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Res {
    /// `..` past `/`.
    Escape,
    /// No mount covers it.
    Unmounted,
    At(u8, String),
}

fn model_resolve(path: &str, is_macro: bool) -> Res {
    let joined = if path.starts_with('/') { String::from(path) } else { format!("/pkg/{path}") };
    let mut segments: StdVec<&str> = StdVec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Res::Escape;
                }
            }
            s => segments.push(s),
        }
    }
    let root = match segments.first() {
        Some(&"tree") => 0,
        Some(&"pkg") => 1,
        Some(&"out") if !is_macro => 2,
        _ => return Res::Unmounted,
    };
    Res::At(root, segments[1..].join("/"))
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Fs {
    File(&'static [u8]),
    Dir(StdVec<String>),
    Absent,
}

/// What is really there.
fn true_fs(root: u8, rel: &str) -> Fs {
    let files: &[(&str, &[u8])] = if root == 0 { &TREE } else { &PKG };
    if let Some((_, bytes)) = files.iter().find(|(path, _)| *path == rel) {
        return Fs::File(bytes);
    }
    let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
    let mut names: StdVec<String> =
        files.iter().filter_map(|(path, _)| path.strip_prefix(prefix.as_str())).map(|rest| String::from(rest.split('/').next().unwrap_or(rest))).collect();
    if names.is_empty() {
        return Fs::Absent;
    }
    names.sort();
    names.dedup();
    Fs::Dir(names)
}

/// What the run may see of it.
fn seen_fs(root: u8, rel: &str) -> Fs {
    match true_fs(root, rel) {
        Fs::File(bytes) if model_file(root, rel) => Fs::File(bytes),
        Fs::Dir(names) if model_dir(root, rel) => Fs::Dir(
            names
                .into_iter()
                .filter(|name| {
                    let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                    admitted(root, &child)
                })
                .collect(),
        ),
        _ => Fs::Absent,
    }
}

fn seen_of(fs: &Fs) -> Seen {
    match fs {
        Fs::File(bytes) => Seen::File { digest: Digest(blaze_blake3::hash(bytes)), len: u64::try_from(bytes.len()).unwrap() },
        Fs::Dir(names) => Seen::Listing { digest: listing_digest(names.iter().map(String::as_str)).unwrap() },
        Fs::Absent => Seen::Absent,
    }
}

/// An answer, owned.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Ans {
    Hello,
    File(StdVec<u8>),
    Stat(FileKind, u64),
    Listing(StdVec<StdVec<u8>>),
    Value(Option<StdVec<u8>>),
    Words(StdVec<u8>),
    Clock(u64),
    Random(StdVec<u8>),
    Done,
    Spawned(i32, StdVec<u8>),
    Bridge(StdVec<u8>),
    Error(SysError),
}

impl Ans {
    fn of(answer: &Answer<'_>) -> Ans {
        match *answer {
            Answer::Hello { .. } => Ans::Hello,
            Answer::File { bytes } => Ans::File(bytes.to_vec()),
            Answer::Stat { kind, len } => Ans::Stat(kind, len),
            Answer::Listing { names } => Ans::Listing(names.iter().map(<[u8]>::to_vec).collect()),
            Answer::Value { value } => Ans::Value(value.map(<[u8]>::to_vec)),
            Answer::Words { words } => Ans::Words(words.as_bytes().to_vec()),
            Answer::Clock { ns } => Ans::Clock(ns),
            Answer::Random { bytes } => Ans::Random(bytes.to_vec()),
            Answer::Done => Ans::Done,
            Answer::Spawned { status, stdout, .. } => Ans::Spawned(status, stdout.to_vec()),
            Answer::Bridge { reply } => Ans::Bridge(reply.to_vec()),
            Answer::Error(error) => Ans::Error(error),
        }
    }

    fn has_content(&self) -> bool {
        matches!(self, Ans::File(_) | Ans::Stat(..) | Ans::Listing(_) | Ans::Value(Some(_)))
    }
}

/// A drawn call of the model guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum G {
    Look(&'static str, Want),
    Env(&'static str),
    EnvAll,
    Args,
    Cwd,
    Clock(ClockKind),
    Random(u32),
    Write(Stream),
    WriteFile(&'static str),
    CreateDir(&'static str),
    Remove(&'static str),
    Rename(&'static str, &'static str),
    Sleep,
    Thread,
    Socket,
    Spawn,
    Bridge,
    TrackPath(&'static str),
    TrackEnv(&'static str),
}

impl G {
    fn call(self) -> Call<'static> {
        match self {
            G::Look(path, want) => Call::Look { path: path.as_bytes(), want },
            G::Env(name) => Call::Env { name: name.as_bytes() },
            G::EnvAll => Call::EnvAll,
            G::Args => Call::Args,
            G::Cwd => Call::Cwd,
            G::Clock(which) => Call::Clock { which },
            G::Random(len) => Call::Random { len },
            G::Write(stream) => Call::Write { stream, bytes: b"line\n" },
            G::WriteFile(path) => Call::WriteFile { path: path.as_bytes(), at: 0, bytes: b"pub fn g() {}" },
            G::CreateDir(path) => Call::CreateDir { path: path.as_bytes() },
            G::Remove(path) => Call::Remove { path: path.as_bytes() },
            G::Rename(from, to) => Call::Rename { from: from.as_bytes(), to: to.as_bytes() },
            G::Sleep => Call::Sleep { ns: 5 },
            G::Thread => Call::Thread,
            G::Socket => Call::Socket,
            G::Spawn => Call::Spawn { program: b"cc", args: Words(b"-c\0x.c\0"), env: Words::EMPTY, cwd: b"/pkg" },
            G::Bridge => Call::Bridge { method: 8, args: b"1 + 1" },
            G::TrackPath(path) => Call::TrackPath { path: path.as_bytes() },
            G::TrackEnv(name) => Call::TrackEnv { name: name.as_bytes(), value: None },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    /// `Return` (a macro) or `Exit` (a script).
    Normal,
    /// A frame that is not CBOR.
    Garbage,
    /// The process dies.
    Gone,
    /// Calls on past its fuel.
    Loop,
    /// Blocks past its deadline.
    Stall,
}

#[derive(Clone, Debug)]
struct Case {
    is_macro: bool,
    key: [u8; 32],
    fuel_calls: u32,
    calls: StdVec<G>,
    /// Nanoseconds after each call, and the CPU time reported then, if any.
    steps: StdVec<(u64, Option<u64>)>,
    end: End,
}

fn draw(tape: &Tape) -> Case {
    let is_macro = tape.coin("macro");
    let fuel_calls = if tape.coin("tight fuel") { tape.int_in(2u32..=30, "fuel calls") } else { 64 };
    let path = |t: &Tape| PATHS[t.index(PATHS.len(), "path")];
    let out = |t: &Tape| OUT_PATHS[t.index(OUT_PATHS.len(), "out path")];
    let calls = tape.vec(0..=24, "calls", |t| {
        let kinds = if is_macro { 19 } else { 17 };
        match t.index(kinds, "call") {
            0 | 1 | 2 => {
                let want = [Want::Read, Want::Stat, Want::List][t.index(3, "want")];
                G::Look(path(t), want)
            }
            3 => G::Env(NAMES[t.index(NAMES.len(), "name")]),
            4 => G::EnvAll,
            5 => G::Args,
            6 => G::Cwd,
            7 => G::Clock(if t.coin("wall") { ClockKind::Wall } else { ClockKind::Monotonic }),
            8 => G::Random(t.int_in(0u32..=64, "random len")),
            9 => G::Write(if t.coin("stdout") { Stream::Out } else { Stream::Err }),
            10 => G::WriteFile(out(t)),
            11 => G::CreateDir(out(t)),
            12 => G::Remove(out(t)),
            13 => G::Rename(out(t), out(t)),
            14 => G::Sleep,
            15 => {
                if t.coin("thread") {
                    G::Thread
                } else {
                    G::Socket
                }
            }
            16 => G::Spawn,
            17 => G::TrackPath(path(t)),
            _ => {
                if t.coin("bridge") {
                    G::Bridge
                } else {
                    G::TrackEnv(NAMES[t.index(NAMES.len(), "tracked name")])
                }
            }
        }
    });
    let steps = calls
        .iter()
        .map(|_| (tape.int_in(0u64..=1000, "step ns"), if tape.one_in(16, "cpu") { Some(tape.int_in(0u64..=2 * CPU_FUEL, "cpu ns")) } else { None }))
        .collect();
    let end = [End::Normal, End::Normal, End::Normal, End::Garbage, End::Gone, End::Loop, End::Stall][tape.index(7, "end")];
    let mut key = [0u8; 32];
    key[0] = tape.int_in(0u8..=255, "key");
    Case { is_macro, key, fuel_calls, calls, steps, end }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum EndSeen {
    Finished,
    Fuel,
    Deadline,
    Wire,
    Gone,
    Other(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct DoneSeen {
    output: StdVec<u8>,
    exit: Option<i32>,
    reads: StdVec<(u8, String, Seen)>,
    reads_digest: Digest,
    facts_digest: Digest,
    files: StdVec<(StdVec<u8>, StdVec<u8>, bool)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Log {
    /// Each answered call: its index in the case and its answer.
    answers: StdVec<(usize, Ans)>,
    findings: StdVec<(StdVec<u8>, Grant)>,
    end: EndSeen,
    done: Option<DoneSeen>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum CompBroken {
    /// A lookup or a variable answered with content the model withholds, or other content.
    Leaked { call: String, got: String, expected: String },
    /// An answer without content that differs from the model's.
    Wrong { call: String, got: String, expected: String },
    /// The read list is not the answered lookups.
    ReadList { got: String, expected: String },
    /// A clock answer that is not the grants' clock.
    Clock { ns: u64 },
    /// Random bytes that are not the seed's stream.
    Random,
    /// The run ended another way than the case calls for.
    End { got: String, expected: String },
    /// The refusals are not the model's.
    Findings { got: String, expected: String },
    /// Two runs of one case differ.
    Nondeterministic { first: String, second: String },
}

impl fmt::Display for CompBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompBroken::Leaked { call, got, expected } => write!(f, "{call} was answered {got}; the grants allow {expected}"),
            CompBroken::Wrong { call, got, expected } => write!(f, "{call} was answered {got}; the model says {expected}"),
            CompBroken::ReadList { got, expected } => write!(f, "the read list is {got}; what was answered is {expected}"),
            CompBroken::Clock { ns } => write!(f, "a clock read gave {ns}, not the fixed clock"),
            CompBroken::Random => f.write_str("random bytes are not the seed's stream"),
            CompBroken::End { got, expected } => write!(f, "the run ended {got}; the case calls for {expected}"),
            CompBroken::Findings { got, expected } => write!(f, "the findings are {got}; the model's are {expected}"),
            CompBroken::Nondeterministic { first, second } => write!(f, "two runs differ: {first} then {second}"),
        }
    }
}

blaze_sim::plants! {
    impl HostPlant breaks CompBroken {
        None,
        OutsideAnswered => CompBroken::Leaked { .. },
        ReadUnlisted => CompBroken::ReadList { .. },
        HostClock => CompBroken::Clock { .. },
    }
}

blaze_sim::points! {
    enum Reached {
        Finished,
        Fuel,
        Deadline,
        Crashed,
        Refused,
        Listed,
        Wrote,
        Spawned,
    }
}

fn harness<E: fmt::Debug>(error: E) -> Broken<CompBroken> {
    Broken::Harness(format!("{error:?}"))
}

/// Runs the host's effects until it has an answer to send (its frame) or nothing more to do.
fn pump<P: Planted<HostPlant>>(host: &mut Host<'_, '_, P>, now: Nanos) -> Result<Option<StdVec<u8>>, Broken<CompBroken>> {
    enum Next {
        Look(u8, String),
        Bridge,
        Spawn,
    }
    loop {
        let next = match host.next_effect() {
            None | Some(Effect::Kill) => return Ok(None),
            Some(Effect::Send { frame }) => return Ok(Some(frame.to_vec())),
            Some(Effect::Look { root, path }) => Next::Look(root.0, String::from_utf8(path.to_vec()).map_err(harness)?),
            Some(Effect::Bridge { .. }) => Next::Bridge,
            Some(Effect::Spawn { .. }) => Next::Spawn,
        };
        match next {
            Next::Look(root, rel) => {
                let fs = true_fs(root, &rel);
                // The driver lists in any order: the reverse, so the host's sort is exercised.
                let mut words = StdVec::new();
                if let Fs::Dir(names) = &fs {
                    for name in names.iter().rev() {
                        words.extend_from_slice(name.as_bytes());
                        words.push(0);
                    }
                }
                let found = match fs {
                    Fs::File(bytes) => Found::File { bytes },
                    Fs::Dir(_) => Found::Dir { names: Words(&words) },
                    Fs::Absent => Found::Absent,
                };
                host.step(now, Event::Looked(found)).map_err(harness)?;
            }
            Next::Bridge => host.step(now, Event::Bridged { reply: b"reply" }).map_err(harness)?,
            Next::Spawn => host.step(now, Event::Spawned { status: 0, stdout: b"cc ok", stderr: b"" }).map_err(harness)?,
        }
    }
}

fn body(frame: &[u8]) -> Result<&[u8], Broken<CompBroken>> {
    match split_frame(frame, crate::WireLimits::SHIPPED).map_err(harness)? {
        Some((body, used)) if used == frame.len() => Ok(body),
        other => Err(Broken::Harness(format!("not one whole frame: {other:?}"))),
    }
}

/// One call over the wire: its answer, or `None` when the host ended instead of answering.
fn exchange<P: Planted<HostPlant>>(
    host: &mut Host<'_, '_, P>,
    guest: &mut Guest<'_>,
    now: Nanos,
    call: &Call<'_>,
) -> Result<Option<Ans>, Broken<CompBroken>> {
    guest.call(call).map_err(harness)?;
    let frame = match guest.next_effect() {
        Some(GuestEffect::Send { frame }) => frame.to_vec(),
        None => return Err(Broken::Harness(String::from("the guest sent nothing"))),
    };
    host.step(now, Event::Frame(body(&frame)?)).map_err(harness)?;
    let Some(answer) = pump(host, now)? else { return Ok(None) };
    guest.step(now, GuestEvent::Frame(body(&answer)?)).map_err(harness)?;
    Ok(Some(Ans::of(&guest.answer().map_err(harness)?)))
}

fn run(case: &Case, plant: impl Planted<HostPlant>, t0: u64) -> Result<Log, Broken<CompBroken>> {
    let globs = [Glob::new("Cargo.toml").map_err(harness)?, Glob::new("src/**").map_err(harness)?, Glob::new("abi/*.json").map_err(harness)?];
    let root_list = [
        Root { name: "tree", kind: RootKind::Tree { tree: Digest([3; 32]) } },
        Root { name: "pkg", kind: RootKind::Package { globs: &globs } },
        Root { name: "out", kind: RootKind::Output },
    ];
    let mount_list = [
        Mount { spelling: b"/tree", root: RootId(0) },
        Mount { spelling: b"/pkg", root: RootId(1) },
        Mount { spelling: b"/out", root: RootId(2) },
    ];
    let given = if case.is_macro { 2 } else { 3 };
    let roots = Roots::new(&root_list[..given]).map_err(harness)?;
    let mounts = Mounts::new(&mount_list[..given], Some(b"/pkg"), &roots).map_err(harness)?;
    let env = [
        EnvGrant { name: b"CARGO_PKG_NAME", value: EnvValue::Set(b"demo") },
        EnvGrant { name: b"HOME", value: EnvValue::Unset },
        EnvGrant { name: b"OUT_DIR", value: EnvValue::Set(b"/out") },
    ];
    let fuel = Fuel { instructions: u64::MAX, cpu_ns: CPU_FUEL, calls: case.fuel_calls };
    let deadline = Deadline { within_ns: DEADLINE };
    let key = CallKey(case.key);
    let grants = if case.is_macro {
        Grants::proc_macro(ProcMacroOpts {
            key,
            name: b"derive_demo",
            input: b"struct S;",
            attr: None,
            roots,
            mounts,
            env: &env[..2],
            fuel,
            deadline,
            ..
        })
    } else {
        Grants::build_script(BuildScriptOpts { key, roots, mounts, env: &env, fuel, deadline, .. })
    }
    .map_err(harness)?;
    let mut host = Host::new_planted(HostOpts { grants, alloc: heap(), .. }, plant);
    let mut guest = Guest::new(GuestOpts { alloc: heap(), .. });
    let mut now = t0;
    let mut answers = StdVec::new();
    let mut live = exchange(&mut host, &mut guest, Nanos(now), &Call::Hello { protocol: crate::PROTOCOL })?.is_some();
    for (at, g) in case.calls.iter().enumerate() {
        if !live {
            break;
        }
        match exchange(&mut host, &mut guest, Nanos(now), &g.call())? {
            Some(answer) => answers.push((at, answer)),
            None => live = false,
        }
        let (step, cpu) = case.steps[at];
        now += step;
        if live && let Some(used_ns) = cpu {
            host.step(Nanos(now), Event::Cpu { used_ns }).map_err(harness)?;
            live = !host.state().ended();
        }
    }
    if live {
        match case.end {
            End::Normal => {
                let last = if case.is_macro { Call::Return { output: b"tokens" } } else { Call::Exit { code: 0 } };
                if exchange(&mut host, &mut guest, Nanos(now), &last)?.is_some() {
                    return Err(Broken::Harness(String::from("an answer to the run's last call")));
                }
            }
            End::Garbage => host.step(Nanos(now), Event::Frame(&[0xff, 0x00])).map_err(harness)?,
            End::Gone => host.step(Nanos(now), Event::Gone).map_err(harness)?,
            End::Loop => {
                let mut rounds = 0;
                while exchange(&mut host, &mut guest, Nanos(now), &Call::Clock { which: ClockKind::Monotonic })?.is_some() {
                    rounds += 1;
                    if rounds > 1000 {
                        return Err(Broken::Harness(String::from("a looping guest was never stopped")));
                    }
                }
            }
            End::Stall => {
                now += DEADLINE;
                host.step(Nanos(now), Event::Tick).map_err(harness)?;
            }
        }
    }
    let findings = host.findings().map(|f| (f.name.to_vec(), f.grant)).collect();
    let (end, done) = match host.finish() {
        Ok(done) => (EndSeen::Finished, Some(done_seen(&done))),
        Err(Ended::Fuel { .. }) => (EndSeen::Fuel, None),
        Err(Ended::Deadline { .. }) => (EndSeen::Deadline, None),
        Err(Ended::Crashed(crate::Crash::Wire(_))) => (EndSeen::Wire, None),
        Err(Ended::Crashed(crate::Crash::Gone)) => (EndSeen::Gone, None),
        Err(other) => (EndSeen::Other(format!("{other:?}")), None),
    };
    Ok(Log { answers, findings, end, done })
}

fn done_seen(done: &Done<'_>) -> DoneSeen {
    DoneSeen {
        output: done.output.as_slice().to_vec(),
        exit: done.exit,
        reads: done.reads.entries().map(|e| (e.root.0, String::from(e.path.as_str()), e.seen)).collect(),
        reads_digest: done.reads.digest(),
        facts_digest: done.facts.digest(),
        files: done.files.iter().map(|f| (f.path.as_slice().to_vec(), f.data.as_slice().to_vec(), f.dir)).collect(),
    }
}

fn expect_look(g: G, is_macro: bool) -> Option<Ans> {
    let G::Look(path, want) = g else { return None };
    let (root, rel) = match model_resolve(path, is_macro) {
        Res::Escape | Res::Unmounted => return Some(Ans::Error(SysError::NotFound)),
        Res::At(2, _) => return None,
        Res::At(root, rel) => (root, rel),
    };
    if !admitted(root, &rel) {
        return Some(Ans::Error(SysError::NotFound));
    }
    Some(match (seen_fs(root, &rel), want) {
        (Fs::File(bytes), Want::Read) => Ans::File(bytes.to_vec()),
        (Fs::File(bytes), Want::Stat) => Ans::Stat(FileKind::File, u64::try_from(bytes.len()).unwrap()),
        (Fs::File(_), Want::List) => Ans::Error(SysError::NotADir),
        (Fs::Dir(_), Want::Read) => Ans::Error(SysError::IsADir),
        (Fs::Dir(_), Want::Stat) => Ans::Stat(FileKind::Dir, 0),
        (Fs::Dir(names), Want::List) => Ans::Listing(names.into_iter().map(String::into_bytes).collect()),
        (Fs::Absent, _) => Ans::Error(SysError::NotFound),
    })
}

fn env_value(name: &str, is_macro: bool) -> Option<StdVec<u8>> {
    match name {
        "CARGO_PKG_NAME" => Some(b"demo".to_vec()),
        "OUT_DIR" if !is_macro => Some(b"/out".to_vec()),
        _ => None,
    }
}

fn granted(name: &str, is_macro: bool) -> bool {
    matches!(name, "CARGO_PKG_NAME" | "HOME") || (name == "OUT_DIR" && !is_macro)
}

/// The refusal a call makes, if any, by the model.
fn expect_finding(g: G, is_macro: bool) -> Option<(StdVec<u8>, Grant)> {
    let named = |s: &str, grant| Some((s.as_bytes().to_vec(), grant));
    match g {
        G::Look(path, _) | G::TrackPath(path) => match model_resolve(path, is_macro) {
            Res::Escape => named(path, Grant::Unaddressable),
            Res::Unmounted => named(path, Grant::Mount),
            Res::At(2, _) => None,
            Res::At(root, rel) if !admitted(root, &rel) => named(path, Grant::Glob { root: RootId(root) }),
            Res::At(..) => None,
        },
        G::Env(name) | G::TrackEnv(name) if !granted(name, is_macro) => named(name, Grant::Env),
        G::WriteFile(path) | G::CreateDir(path) | G::Remove(path) => match model_resolve(path, is_macro) {
            Res::Escape => named(path, Grant::Unaddressable),
            Res::At(2, _) => None,
            _ => named(path, Grant::Write),
        },
        G::Rename(from, to) => match model_resolve(from, is_macro) {
            Res::Escape => named(from, Grant::Unaddressable),
            Res::At(2, _) => match model_resolve(to, is_macro) {
                Res::Escape => named(to, Grant::Unaddressable),
                Res::At(2, _) => None,
                _ => named(to, Grant::Write),
            },
            _ => named(from, Grant::Write),
        },
        G::Thread => named("pthread_create", Grant::Thread),
        G::Socket => named("socket", Grant::Network),
        G::Spawn if is_macro => named("cc", Grant::Spawn),
        _ => None,
    }
}

fn expect_end(case: &Case) -> EndSeen {
    let mut frames: u32 = 1;
    for (at, _) in case.calls.iter().enumerate() {
        frames += 1;
        if frames > case.fuel_calls {
            return EndSeen::Fuel;
        }
        if case.steps[at].1.is_some_and(|cpu| cpu > CPU_FUEL) {
            return EndSeen::Fuel;
        }
    }
    match case.end {
        End::Normal if frames + 1 > case.fuel_calls => EndSeen::Fuel,
        End::Normal => EndSeen::Finished,
        End::Garbage if frames + 1 > case.fuel_calls => EndSeen::Fuel,
        End::Garbage => EndSeen::Wire,
        End::Gone => EndSeen::Gone,
        End::Loop => EndSeen::Fuel,
        End::Stall => EndSeen::Deadline,
    }
}

fn ensure(holds: bool, broken: impl FnOnce() -> CompBroken) -> Result<(), Broken<CompBroken>> {
    if holds { Ok(()) } else { Err(Broken::Oracle(broken())) }
}

fn comptime_case(tape: &Tape, plant: impl Planted<HostPlant>) -> Result<(), Broken<CompBroken>> {
    let case = draw(tape);
    let log = run(&case, plant, 1_000)?;
    let is_macro = case.is_macro;

    // Every answer against the model; the random stream against the seed's.
    let mut xof = blaze_blake3::Hasher::new_keyed(&Seed::of(CallKey(case.key)).0).finalize_xof();
    let mut expected_reads: StdVec<(u8, String, Seen)> = StdVec::new();
    let mut expected_findings = StdVec::new();
    for (at, answer) in &log.answers {
        let g = case.calls[*at];
        let call = format!("{g:?}");
        let expected = match g {
            G::Look(..) => expect_look(g, is_macro),
            G::Env(name) => Some(Ans::Value(env_value(name, is_macro))),
            G::Cwd => Some(Ans::Value(Some(b"/pkg".to_vec()))),
            G::Args => Some(Ans::Words(if is_macro { StdVec::new() } else { b"build-script-build\0".to_vec() })),
            _ => None,
        };
        if let Some(expected) = expected
            && *answer != expected
        {
            let (got, expected) = (format!("{answer:?}"), format!("{expected:?}"));
            return Err(Broken::Oracle(if answer.has_content() {
                CompBroken::Leaked { call, got, expected }
            } else {
                CompBroken::Wrong { call, got, expected }
            }));
        }
        match (g, answer) {
            (G::Clock(_), Ans::Clock(ns)) => ensure(*ns == 0, || CompBroken::Clock { ns: *ns })?,
            (G::Random(len), Ans::Random(bytes)) => {
                let mut want = vec![0u8; usize::try_from(len).unwrap()];
                xof.fill(&mut want);
                ensure(*bytes == want, || CompBroken::Random)?;
            }
            (G::Look(..), Ans::Listing(_)) => tape.reach(Reached::Listed),
            (G::WriteFile(_), Ans::Done) => tape.reach(Reached::Wrote),
            (G::Spawn, Ans::Spawned(..)) => tape.reach(Reached::Spawned),
            _ => {}
        }
        if let G::Look(path, _) | G::TrackPath(path) = g
            && let Res::At(root @ (0 | 1), rel) = model_resolve(path, is_macro)
            && admitted(root, &rel)
        {
            let fs = seen_fs(root, &rel);
            // A directory stat'ed or opened is seen to exist; only a listing lists it.
            let seen = match (g, &fs) {
                (G::Look(_, Want::Stat | Want::Read), Fs::Dir(_)) => Seen::Dir,
                _ => seen_of(&fs),
            };
            expected_reads.push((root, rel, seen));
        }
        if let Some(finding) = expect_finding(g, is_macro) {
            expected_findings.push(finding);
        }
    }
    ensure(log.findings == expected_findings, || CompBroken::Findings {
        got: format!("{:?}", log.findings),
        expected: format!("{expected_findings:?}"),
    })?;
    if !log.findings.is_empty() {
        tape.reach(Reached::Refused);
    }

    // The end the case calls for; output only from a finished run.
    let expected_end = expect_end(&case);
    ensure(log.end == expected_end, || CompBroken::End { got: format!("{:?}", log.end), expected: format!("{expected_end:?}") })?;
    match log.end {
        EndSeen::Finished => tape.reach(Reached::Finished),
        EndSeen::Fuel => tape.reach(Reached::Fuel),
        EndSeen::Deadline => tape.reach(Reached::Deadline),
        _ => tape.reach(Reached::Crashed),
    }

    // The read list: exactly what was answered.
    if let Some(done) = &log.done {
        let key = |e: &(u8, String, Seen)| {
            let (rank, digest) = match e.2 {
                Seen::File { digest, .. } => (0, digest),
                Seen::Absent => (1, Digest([0; 32])),
                Seen::Listing { digest } => (2, digest),
                Seen::Dir => (3, Digest([0; 32])),
            };
            (e.0, e.1.clone(), rank, digest)
        };
        // A listing of a path covers its being seen to exist (blaze-roots list.rs).
        let listed: StdVec<(u8, String)> =
            expected_reads.iter().filter(|e| matches!(e.2, Seen::Listing { .. })).map(|e| (e.0, e.1.clone())).collect();
        expected_reads.retain(|e| e.2 != Seen::Dir || !listed.iter().any(|l| l.0 == e.0 && l.1 == e.1));
        expected_reads.sort_by_key(key);
        expected_reads.dedup();
        ensure(done.reads == expected_reads, || CompBroken::ReadList {
            got: format!("{:?}", done.reads),
            expected: format!("{expected_reads:?}"),
        })?;
        if is_macro {
            ensure(done.output == b"tokens", || CompBroken::End { got: format!("{:?}", done.output), expected: String::from("tokens") })?;
        }
    }

    // The same case at another host time: the same run.
    let again = run(&case, plant, 7_777_777)?;
    ensure(again == log, || CompBroken::Nondeterministic { first: format!("{log:?}"), second: format!("{again:?}") })
}

#[test]
fn a_comptime_run_answers_only_from_its_grants_and_lists_what_it_read() {
    check(Config::cases(3000).must_reach_points(<Reached as blaze_sim::Point>::IDS), |tape| comptime_case(tape, blaze_sim::NoPlant));
}

#[test]
fn every_host_plant_is_caught_by_its_invariant() {
    controls(Config::cases(3000), |tape, plant: HostPlant| comptime_case(tape, plant));
}

#[test]
fn every_call_and_answer_round_trips_the_wire() {
    use crate::wire::{read_answer, read_call, write_answer, write_call};
    let limits = crate::WireLimits::SHIPPED;
    let calls = [
        Call::Hello { protocol: 1 },
        Call::Look { path: b"src/lib.rs", want: Want::List },
        Call::Env { name: b"HOME" },
        Call::EnvAll,
        Call::Args,
        Call::Cwd,
        Call::Clock { which: ClockKind::Wall },
        Call::Random { len: 9 },
        Call::Write { stream: Stream::Err, bytes: b"x" },
        Call::WriteFile { path: b"/out/a", at: 7, bytes: b"y" },
        Call::SetLen { path: b"/out/a", len: 3 },
        Call::CreateDir { path: b"/out/d" },
        Call::Remove { path: b"/out/d" },
        Call::Rename { from: b"/out/a", to: b"/out/b" },
        Call::Sleep { ns: 1 },
        Call::Thread,
        Call::Socket,
        Call::Spawn { program: b"cc", args: Words(b"-c\0"), env: Words::EMPTY, cwd: b"/pkg" },
        Call::Bridge { method: 9, args: b"" },
        Call::TrackEnv { name: b"A", value: Some(b"1") },
        Call::TrackPath { path: b"a.sol" },
        Call::Return { output: b"t" },
        Call::Exit { code: -3 },
        Call::Denied { name: b"chmod" },
    ];
    let mut out = blaze_own::Vec::new_in(heap());
    for call in calls {
        out.clear();
        write_call(&call, &mut out, limits).unwrap();
        let (body, _) = split_frame(&out, limits).unwrap().unwrap();
        assert_eq!(read_call(body, limits), Ok(call));
    }
    let answers = [
        Answer::Hello { protocol: 1 },
        Answer::File { bytes: b"f" },
        Answer::Stat { kind: FileKind::Dir, len: 0 },
        Answer::Listing { names: Words(b"a\0b\0") },
        Answer::Value { value: None },
        Answer::Words { words: Words::EMPTY },
        Answer::Clock { ns: 0 },
        Answer::Random { bytes: b"r" },
        Answer::Done,
        Answer::Spawned { status: -1, stdout: b"o", stderr: b"e" },
        Answer::Bridge { reply: b"b" },
        Answer::Error(SysError::NotEmpty),
    ];
    for answer in answers {
        out.clear();
        write_answer(&answer, &mut out, limits).unwrap();
        let (body, _) = split_frame(&out, limits).unwrap().unwrap();
        assert_eq!(read_answer(body, limits), Ok(answer));
    }
    // Every length is capped before anything is sized by it.
    let small = crate::WireLimits { most_name: 3, ..limits };
    out.clear();
    write_call(&Call::Env { name: b"LONG" }, &mut out, limits).unwrap();
    let (body, _) = split_frame(&out, limits).unwrap().unwrap();
    assert!(matches!(read_call(body, small), Err(crate::WireError::Cbor(_))));
    assert!(matches!(split_frame(&[0xff, 0xff, 0xff, 0xff], limits), Err(crate::WireError::Frame { .. })));
}

#[test]
fn the_two_constructors_hold_the_two_kinds_of_run() {
    let globs = [Glob::new("src/**").unwrap()];
    let list = [Root { name: "pkg", kind: RootKind::Package { globs: &globs } }, Root { name: "out", kind: RootKind::Output }];
    let with_out = Roots::new(&list).unwrap();
    let without = Roots::new(&list[..1]).unwrap();
    let key = CallKey([1; 32]);
    let env = [EnvGrant { name: b"B", value: EnvValue::Unset }, EnvGrant { name: b"A", value: EnvValue::Unset }];
    fn macro_opts<'g>(roots: Roots<'g>, env: &'g [EnvGrant<'g>]) -> ProcMacroOpts<'g> {
        ProcMacroOpts { key: CallKey([1; 32]), name: b"m", input: b"", attr: None, roots, mounts: Mounts::NONE, env, .. }
    }
    fn script(roots: Roots<'_>) -> BuildScriptOpts<'_> {
        BuildScriptOpts { key: CallKey([1; 32]), roots, mounts: Mounts::NONE, env: &[], .. }
    }
    assert_eq!(Grants::proc_macro(macro_opts(with_out, &[])).err(), Some(crate::GrantsError::MacroOutput));
    assert_eq!(Grants::proc_macro(macro_opts(without, &env)).err(), Some(crate::GrantsError::EnvOrder { at: 1 }));
    let grants = Grants::proc_macro(macro_opts(without, &env[1..])).unwrap();
    assert_eq!((grants.clock, grants.spawns, grants.args.len()), (crate::Clock::FIXED, crate::Spawns::None, 0));
    assert_eq!(grants.seed, Seed::of(key));
    assert_eq!(Grants::build_script(script(without)).err(), Some(crate::GrantsError::NoOutput));
    let grants = Grants::build_script(script(with_out)).unwrap();
    assert_eq!((grants.spawns, grants.args), (crate::Spawns::CMode, &[&b"build-script-build"[..]][..]));
    assert_eq!(grants.kind, crate::RunKind::BuildScript);
}

#[test]
fn guest_fits_accepts_too_large_on_bulk_calls_and_wire_caps_random() {
    use crate::CallKind as K;
    use crate::guest::fits;
    use crate::wire::{read_answer, write_answer};

    for kind in [K::EnvAll, K::Args, K::Cwd, K::Random] {
        assert!(fits(kind, &Answer::Error(SysError::TooLarge)), "{kind:?} must accept TooLarge");
        assert!(!fits(kind, &Answer::Error(SysError::NotFound)), "{kind:?} must still reject NotFound");
    }
    assert!(!fits(K::Hello, &Answer::Error(SysError::TooLarge)));
    assert!(!fits(K::Clock, &Answer::Error(SysError::TooLarge)));

    let big = crate::WireLimits::SHIPPED;
    let small_rand = crate::WireLimits { most_random: 4, ..big };
    let mut out = blaze_own::Vec::new_in(heap());
    write_answer(&Answer::Random { bytes: b"12345" }, &mut out, big).unwrap();
    let (body, _) = split_frame(&out, big).unwrap().unwrap();
    assert!(matches!(read_answer(body, small_rand), Err(crate::WireError::Cbor(_))));
    assert_eq!(read_answer(body, big), Ok(Answer::Random { bytes: b"12345" }));
}

#[test]
fn nospace_does_not_leak_ghost_output_file() {
    let globs = [Glob::new("src/**").unwrap()];
    let list = [Root { name: "pkg", kind: RootKind::Package { globs: &globs } }, Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = Mounts::new(&[Mount { spelling: b"/pkg", root: RootId(0) }, Mount { spelling: b"/out", root: RootId(1) }], Some(b"/pkg"), &roots).unwrap();
    let output = crate::OutputCaps { most_file_bytes: 4, most_files: 4, ..crate::OutputCaps::SCRIPT };
    let grants = Grants::build_script(BuildScriptOpts { key: CallKey([7; 32]), roots, mounts, env: &[], output, .. }).unwrap();

    let mut host = Host::new(HostOpts { grants, alloc: heap(), .. });
    let mut guest = Guest::new(GuestOpts { alloc: heap(), .. });
    assert_eq!(exchange(&mut host, &mut guest, Nanos(0), &Call::Hello { protocol: crate::PROTOCOL }).unwrap(), Some(Ans::Hello));

    // WriteFile exceeding most_file_bytes returns NoSpace and must NOT create a ghost 0-byte file.
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(1), &Call::WriteFile { path: b"/out/big", at: 0, bytes: b"12345" }).unwrap(),
        Some(Ans::Error(SysError::NoSpace)),
    );
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(2), &Call::Look { path: b"/out/big", want: Want::Stat }).unwrap(),
        Some(Ans::Error(SysError::NotFound)),
    );

    // Same for SetLen on a new file.
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(3), &Call::SetLen { path: b"/out/big2", len: 10 }).unwrap(),
        Some(Ans::Error(SysError::NoSpace)),
    );
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(4), &Call::Look { path: b"/out/big2", want: Want::Stat }).unwrap(),
        Some(Ans::Error(SysError::NotFound)),
    );

    // Overflowing offset + len returns NoSpace cleanly.
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(5), &Call::WriteFile { path: b"/out/ovf", at: u64::MAX, bytes: b"x" }).unwrap(),
        Some(Ans::Error(SysError::NoSpace)),
    );

    assert_eq!(exchange(&mut host, &mut guest, Nanos(6), &Call::Exit { code: 0 }).unwrap(), None);
    let done = host.finish().unwrap();
    assert!(done.files.is_empty(), "no ghost files may remain after NoSpace");
}

#[test]
fn rename_outside_output_root_records_finding_and_spawn_fact_key_has_no_nul_collision() {
    let globs = [Glob::new("src/**").unwrap()];
    let list = [Root { name: "pkg", kind: RootKind::Package { globs: &globs } }, Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = Mounts::new(&[Mount { spelling: b"/pkg", root: RootId(0) }, Mount { spelling: b"/out", root: RootId(1) }], Some(b"/pkg"), &roots).unwrap();
    let grants = Grants::build_script(BuildScriptOpts { key: CallKey([8; 32]), roots, mounts, env: &[], .. }).unwrap();

    let mut host = Host::new(HostOpts { grants, alloc: heap(), .. });
    let mut guest = Guest::new(GuestOpts { alloc: heap(), .. });
    exchange(&mut host, &mut guest, Nanos(0), &Call::Hello { protocol: crate::PROTOCOL }).unwrap();

    // Create `/out/a`, attempt to rename into `/pkg/x` (outside output root): must record Finding.
    assert_eq!(exchange(&mut host, &mut guest, Nanos(1), &Call::WriteFile { path: b"/out/a", at: 0, bytes: b"hi" }).unwrap(), Some(Ans::Done));
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(2), &Call::Rename { from: b"/out/a", to: b"/pkg/x" }).unwrap(),
        Some(Ans::Error(SysError::ReadOnly)),
    );
    let findings: StdVec<_> = host.findings().map(|f| (f.call, f.name.to_vec(), f.grant)).collect();
    assert_eq!(findings, vec![(crate::CallKind::Rename, b"/pkg/x".to_vec(), Grant::Write)]);

    // Two spawns that would collide under NUL-only concatenation must produce distinct FactKind::Spawn entries.
    let s1 = Call::Spawn { program: b"a", args: Words(b"b\0"), env: Words::EMPTY, cwd: b"/pkg" };
    let s2 = Call::Spawn { program: b"a", args: Words::EMPTY, env: Words(b"b\0"), cwd: b"/pkg" };
    exchange(&mut host, &mut guest, Nanos(3), &s1).unwrap();
    exchange(&mut host, &mut guest, Nanos(4), &s2).unwrap();

    exchange(&mut host, &mut guest, Nanos(5), &Call::Exit { code: 0 }).unwrap();
    let done = host.finish().unwrap();
    let spawns: StdVec<_> = done.facts.iter().filter(|f| f.kind == crate::FactKind::Spawn).collect();
    assert_eq!(spawns.len(), 2, "length-prefixed spawn keys must not collide");
    assert_ne!(spawns[0].name, spawns[1].name);
}

#[test]
fn stderr_cap_and_facts_findings_caps_and_sorted_output_root() {
    let globs = [Glob::new("src/**").unwrap()];
    let list = [Root { name: "pkg", kind: RootKind::Package { globs: &globs } }, Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = Mounts::new(&[Mount { spelling: b"/pkg", root: RootId(0) }, Mount { spelling: b"/out", root: RootId(1) }], Some(b"/pkg"), &roots).unwrap();
    let output = crate::OutputCaps { most_log: 8, most_file_bytes: 16, most_files: 16, ..crate::OutputCaps::SCRIPT };
    let limits = blaze_roots::Limits { most_entries: 2, most_path: 64, most_text: 64 };
    let grants = Grants::build_script(BuildScriptOpts { key: CallKey([9; 32]), roots, mounts, env: &[], output, limits, .. }).unwrap();

    let mut host = Host::new(HostOpts { grants, alloc: heap(), .. });
    let mut guest = Guest::new(GuestOpts { alloc: heap(), .. });
    exchange(&mut host, &mut guest, Nanos(0), &Call::Hello { protocol: crate::PROTOCOL }).unwrap();

    // Stream::Err large write slices down to `most_log = 8` without allocating oversized buffer.
    let big: StdVec<u8> = (0..1024u32).map(|i| b'a' + (i % 26) as u8).collect();
    assert_eq!(exchange(&mut host, &mut guest, Nanos(1), &Call::Write { stream: Stream::Err, bytes: &big }).unwrap(), Some(Ans::Done));
    assert_eq!(exchange(&mut host, &mut guest, Nanos(2), &Call::Write { stream: Stream::Err, bytes: b"1234" }).unwrap(), Some(Ans::Done));

    // Repeated Env("X") deduplicates in Facts; Findings caps cleanly at `most_entries = 2`.
    for i in 0..5u64 {
        assert_eq!(exchange(&mut host, &mut guest, Nanos(10 + i), &Call::Env { name: b"X" }).unwrap(), Some(Ans::Value(None)));
    }
    assert_eq!(host.findings().count(), 2, "findings capped at most_entries without panicking");

    // Sorted output root prefix edge cases: `a-1` and `a.txt` sort between `a` and `a/child`.
    for call in [
        Call::CreateDir { path: b"/out/a" },
        Call::WriteFile { path: b"/out/a-1", at: 0, bytes: b"1234" },
        Call::WriteFile { path: b"/out/a.txt", at: 0, bytes: b"5678" },
        Call::WriteFile { path: b"/out/a/child", at: 0, bytes: b"90" },
        Call::WriteFile { path: b"/out/a0", at: 0, bytes: b"xy" },
    ] {
        assert_eq!(exchange(&mut host, &mut guest, Nanos(20), &call).unwrap(), Some(Ans::Done));
    }
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(21), &Call::Remove { path: b"/out/a" }).unwrap(),
        Some(Ans::Error(SysError::NotEmpty)),
    );
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(22), &Call::Look { path: b"/out/a", want: Want::List }).unwrap(),
        Some(Ans::Listing(vec![b"child".to_vec()])),
    );
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(23), &Call::Look { path: b"/out", want: Want::List }).unwrap(),
        Some(Ans::Listing(vec![b"a".to_vec(), b"a-1".to_vec(), b"a.txt".to_vec(), b"a0".to_vec()])),
    );
    // Remove `/out/a/child` and `/out/a`, reclaiming bytes for incremental `out_bytes`.
    assert_eq!(exchange(&mut host, &mut guest, Nanos(24), &Call::Remove { path: b"/out/a/child" }).unwrap(), Some(Ans::Done));
    assert_eq!(exchange(&mut host, &mut guest, Nanos(25), &Call::Remove { path: b"/out/a" }).unwrap(), Some(Ans::Done));
    // Currently 4 + 4 + 2 = 10 bytes used out of 16; writing 6 bytes fits, 7 does not.
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(26), &Call::WriteFile { path: b"/out/z", at: 0, bytes: b"1234567" }).unwrap(),
        Some(Ans::Error(SysError::NoSpace)),
    );
    assert_eq!(
        exchange(&mut host, &mut guest, Nanos(27), &Call::WriteFile { path: b"/out/z", at: 0, bytes: b"123456" }).unwrap(),
        Some(Ans::Done),
    );

    exchange(&mut host, &mut guest, Nanos(30), &Call::Exit { code: 0 }).unwrap();
    let done = host.finish().unwrap();
    let mut want_log = big[big.len() - 4..].to_vec();
    want_log.extend_from_slice(b"1234");
    assert_eq!(done.log.as_slice(), want_log.as_slice());
    assert_eq!(done.facts.len(), 1, "repeated Env(X) deduplicated to 1 fact");
    let paths: StdVec<&[u8]> = done.files.iter().map(|f| f.path.as_slice()).collect();
    assert_eq!(paths, vec![&b"a-1"[..], &b"a.txt"[..], &b"a0"[..], &b"z"[..]]);
}
