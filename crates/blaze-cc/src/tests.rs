//! The crate's promises, each with its oracle and, for the stateful pieces, a planted bug:
//!
//! - a unit's arguments hold guest paths alone, the same bytes every time;
//! - the messages are the bytes the table in wire.rs says (hand-written samples, which
//!   helper/wire.h is held to);
//! - the job resolves every lookup to the root and path the system would open, refuses every
//!   path under no mount or outside its root, and its read list is exactly what was found and
//!   what was not (planted: misses forgotten, `..` dropped);
//! - a `cfront-decls 2` document is the model it spells, reads the same however it is split
//!   (planted: a split line restarts), and every malformed one is refused at its line.
//!
//! The roots, paths and read list themselves are crates/blaze-roots' and tested there.

use core::fmt;
use std::{format, string::String as StdString, vec, vec::Vec as StdVec};

use blaze_c::{CAbi, DeclsBuilder, DeclsOpts, Endian, IntKind, Root as Place, Scalar, front::abi_row};
use blaze_own::{String, Vec};
use blaze_roots::{Digest, Glob, Mount, Mounts, Outside, Root, RootId, RootKind, Roots, Seen, Unmounted};
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::{
    DeclsPlant, JobPlant,
    decls::{Census, DeclsError, DeclsLimits, DeclsReader, DeclsReaderOpts, DeclsWhy},
    flags::{self, Debug as DebugInfo, Define, FlagFault, Lang, Opt, Source, Target, Toolchain, Unit},
    job::{Effect, Ended, Event, Job, JobError, JobOpts, Limits, Phase},
    wire::{self, Answer, Confinement, FromHelper, Level, LookOp, Mode, OutKind, WireFault},
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> blaze_own::Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const SHELF: RootId = RootId(0);
const PACKAGE: RootId = RootId(1);
const OUT: RootId = RootId(2);

/// The shelf (the pinned clang and the target's headers beneath it, one tree), the package and
/// the output directory, as the helper sees them.
const MOUNTS: [Mount<'static>; 3] = [
    Mount { spelling: b"/shelf", root: SHELF },
    Mount { spelling: b"/package", root: PACKAGE },
    Mount { spelling: b"/out", root: OUT },
];

const TOOLS: Toolchain<'static> = Toolchain { clang: "/shelf/clang", sysroot: "/shelf/sdk", package: "/package" };

const PLACES: [(RootId, Place); 2] = [(SHELF, Place::Shelf), (PACKAGE, Place::Package)];

fn globs() -> [Glob<'static>; 2] {
    [Glob::new("include/**").expect("a glob"), Glob::new("src/*.c").expect("a glob")]
}

/// `body` given the run's roots and mounts.
fn with_roots<R>(body: impl FnOnce(&Roots<'_>, &Mounts<'_>) -> R) -> R {
    let globs = globs();
    let table = [
        Root { name: "shelf", kind: RootKind::Tree { tree: Digest([1; 32]) } },
        Root { name: "package", kind: RootKind::Package { globs: &globs } },
        Root { name: "out", kind: RootKind::Output },
    ];
    let roots = Roots::new(&table).expect("the test's roots are well formed");
    let mounts = Mounts::new(&MOUNTS, None, &roots).expect("the test's mounts are well formed");
    body(&roots, &mounts)
}

fn rel(text: &str) -> blaze_roots::RelPath<'_> {
    blaze_roots::RelPath::new(text, 4096).expect("a test path is relative")
}

// Flags.

#[test]
fn a_units_arguments_are_guest_paths_alone() {
    let include = [rel("include"), rel("include/sub")];
    let defines = [Define { name: "NDEBUG", value: None }, Define { name: "LEVEL", value: Some("2") }];
    let unit = Unit {
        lang: Lang::C,
        std: "c11",
        target: Target::MacosArm64 { min_major: 13, min_minor: 0 },
        source: Source::File(rel("src/main.c")),
        include: &include,
        defines: &defines,
        opt: Opt::Fast,
        debug: DebugInfo::Lines,
    };
    let args = flags::args(&unit, TOOLS, Mode::Compile, heap()).expect("the unit is well formed");
    let got: StdVec<&str> = args.iter().map(|a| a.as_str()).collect();
    assert_eq!(
        got,
        vec![
            "/shelf/clang/bin/clang",
            "--target=aarch64-apple-darwin",
            "--no-default-config",
            "-no-canonical-prefixes",
            "-resource-dir=/shelf/clang/lib/clang/23",
            "-isysroot",
            "/shelf/sdk",
            "-mmacosx-version-min=13.0",
            "-x",
            "c",
            "-std=c11",
            "-I/package/include",
            "-I/package/include/sub",
            "-Wno-builtin-macro-redefined",
            "-D__DATE__=\"Jan  1 1970\"",
            "-D__TIME__=\"00:00:00\"",
            "-D__TIMESTAMP__=\"Thu Jan  1 00:00:00 1970\"",
            "-DNDEBUG",
            "-DLEVEL=2",
            "-O2",
            "-fPIC",
            "-ffunction-sections",
            "-fdata-sections",
            "-fno-common",
            "-gline-tables-only",
            "-fdebug-compilation-dir=/package",
            "-fcoverage-compilation-dir=/package",
            "-gno-record-gcc-switches",
            "-fno-color-diagnostics",
            "-fmessage-length=0",
            "-fno-crash-diagnostics",
            "-c",
            "/package/src/main.c",
        ]
    );
    // Every path named is under a mount.
    with_roots(|_, mounts| {
        for arg in &got {
            if let Some(at) = arg.find('/') {
                let mut out = Vec::new_in(heap());
                let path = arg[at..].trim_end_matches('"');
                assert!(mounts.resolve(path.as_bytes(), 4096, &mut out).is_ok(), "{arg} names a path under no mount");
            }
        }
    });
    let headers = [rel("include/a.h"), rel("include/b.h")];
    let import = Unit { source: Source::Headers(&headers), ..unit };
    let decls = flags::args(&import, TOOLS, Mode::Decls, heap()).expect("an import is well formed");
    let tail: StdVec<&str> = decls.iter().rev().take(6).map(|a| a.as_str()).collect();
    assert_eq!(tail, vec![flags::IMPORT_MAIN, "/package/include/b.h", "-include", "/package/include/a.h", "-include", "-fsyntax-only"]);
    assert_eq!(flags::args(&import, TOOLS, Mode::Compile, heap()).map(|_| ()), Err(FlagFault::Source));
    let cxx = Unit { lang: Lang::Cxx, ..unit };
    assert_eq!(flags::args(&cxx, TOOLS, Mode::Compile, heap()).map(|_| ()), Err(FlagFault::Standard));
    let bad = [Define { name: "1X", value: None }];
    assert_eq!(flags::args(&Unit { defines: &bad, ..unit }, TOOLS, Mode::Decls, heap()).map(|_| ()), Err(FlagFault::DefineName));
    let clock = [Define { name: "__DATE__", value: Some("\"now\"") }];
    assert_eq!(flags::args(&Unit { defines: &clock, ..unit }, TOOLS, Mode::Decls, heap()).map(|_| ()), Err(FlagFault::DefineName));
    let relative = Toolchain { clang: "shelf/clang", ..TOOLS };
    assert_eq!(flags::args(&unit, relative, Mode::Compile, heap()).map(|_| ()), Err(FlagFault::Spelling));
}

// The messages.

#[test]
fn messages_are_the_bytes_the_table_says() {
    assert_eq!(wire::decode(&[0x01, 0, 2, 1]), Ok(FromHelper::Hello { version: 2, confinement: Confinement::Seatbelt }));
    assert_eq!(wire::decode(&[0x01, 0, 2]), Err(WireFault::Short { tag: 1 }));
    assert_eq!(wire::decode(&[0x01, 0, 2, 1, 0]), Err(WireFault::Long { tag: 1 }));
    assert_eq!(
        wire::decode(b"\x02\x00\x00\x00\x07\x02/shelf/sdk/a.h"),
        Ok(FromHelper::Look { id: 7, op: LookOp::Read, path: "/shelf/sdk/a.h" })
    );
    assert_eq!(wire::decode(b"\x02\x00\x00\x00\x08\x03/package"), Ok(FromHelper::Look { id: 8, op: LookOp::List, path: "/package" }));
    assert_eq!(wire::decode(b"\x03\x01obj"), Ok(FromHelper::Out { kind: OutKind::Object, bytes: b"obj" }));
    assert_eq!(wire::decode(b"\x03\x03x"), Err(WireFault::UnknownOut(3)));
    assert_eq!(wire::decode(&[0x04, 1]), Ok(FromHelper::Done { errors: true }));
    assert_eq!(
        wire::decode(b"\x05\x04\x00\x00\x00\x09\x00\x00\x00\x0c\x00\x0c/package/a.hbad"),
        Ok(FromHelper::Diagnostic { level: Level::Error, code: 9, offset: 12, path: "/package/a.h", message: "bad" })
    );
    assert_eq!(wire::decode(b"\x05\x04\x00\x00\x00\x09\x00\x00\x00\x0c\x00\x0d/package/a.h"), Err(WireFault::Short { tag: 5 }));
    assert_eq!(wire::decode(&[0x09]), Err(WireFault::UnknownTag(9)));
    assert_eq!(wire::decode(&[0x02, 0, 0, 0, 1, 4]), Err(WireFault::UnknownOp(4)));

    let mut out = Vec::new_in(heap());
    wire::job(&mut out, Mode::Compile, &["cc", "-c"]).expect("small");
    assert_eq!(out.as_slice(), b"\x00\x00\x00\x10\x81\x02\x00\x02\x00\x00\x00\x02cc\x00\x00\x00\x02-c");
    let (mode, args) = wire::read_job(&out.as_slice()[wire::PREFIX..], heap()).expect("it reads back");
    assert_eq!((mode, args.as_slice()), (Mode::Compile, &["cc", "-c"][..]));

    let answered = |id: u32, answer: Answer<'_>| {
        let mut out = Vec::new_in(heap());
        wire::answer(&mut out, id, &answer).expect("small");
        StdVec::from(out.as_slice())
    };
    assert_eq!(
        answered(5, Answer::File { digest: [0; 32], size: 2, bytes: Some(b"hi") }),
        b"\x00\x00\x00\x10\x82\x00\x00\x00\x05\x01\x00\x00\x00\x00\x00\x00\x00\x02hi"
    );
    assert_eq!(answered(6, Answer::Missing), b"\x00\x00\x00\x06\x82\x00\x00\x00\x06\x00");
    assert_eq!(answered(4, Answer::NotAFile), b"\x00\x00\x00\x06\x82\x00\x00\x00\x04\x04");
    assert_eq!(
        answered(3, Answer::Listing { names: &["a.h", "sub/"] }),
        b"\x00\x00\x00\x15\x82\x00\x00\x00\x03\x03\x00\x00\x00\x02\x00\x03a.h\x00\x04sub/"
    );
}

// The job.

/// A lookup the simulated helper makes, and what the oracle expects of it.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Resolved to this root and path; present or not in the view.
    At { root: RootId, path: &'static str, op: LookOp },
    /// Refused as under no mount, or not a path.
    Unmounted,
    /// Refused as outside its root.
    Outside,
}

/// The view: what is under each root, and whether it is a directory. A file's digest is its
/// index.
const UNIVERSE: &[(RootId, &str, bool)] = &[
    (SHELF, "clang/lib/clang/23/include", true),
    (SHELF, "clang/lib/clang/23/include/stddef.h", false),
    (SHELF, "clang/lib/clang/23/include/stdint.h", false),
    (SHELF, "sdk/usr/include", true),
    (SHELF, "sdk/usr/include/stdio.h", false),
    (SHELF, "sdk/usr/include/sys/types.h", false),
    (PACKAGE, "include", true),
    (PACKAGE, "include/a.h", false),
    (PACKAGE, "include/sub/b.h", false),
    (PACKAGE, "src/main.c", false),
];

fn mount(root: RootId) -> &'static str {
    match root {
        SHELF => "shelf",
        PACKAGE => "package",
        _ => "out",
    }
}

#[derive(Debug)]
enum JobBroken {
    /// A lookup resolved to another root or path than the system would open.
    Resolved { guest: StdString, want: StdString, got: StdString },
    /// A lookup that must be refused was not, or one that must not was.
    Refusal { guest: StdString, got: StdString },
    /// The read list is not what was found and missed.
    Reads { want: StdString, got: StdString },
}

impl fmt::Display for JobBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobBroken::Resolved { guest, want, got } => write!(f, "{guest} resolved to {got}, not {want}"),
            JobBroken::Refusal { guest, got } => write!(f, "{guest}: {got}"),
            JobBroken::Reads { want, got } => write!(f, "the read list is {got}, not {want}"),
        }
    }
}

blaze_sim::plants! {
    impl JobPlant breaks JobBroken {
        None,
        ForgetsMisses => JobBroken::Reads { .. },
        // A dropped `..` reads another file, or one outside its root's globs.
        DropsDotDot => JobBroken::Resolved { .. } | JobBroken::Reads { .. } | JobBroken::Refusal { .. },
    }
}

fn harness(why: &str) -> Broken<JobBroken> {
    Broken::Harness(StdString::from(why))
}

/// A guest path for `path` under `root`, now and then with a `.` or a `zz/..` in it.
fn render(tape: &Tape, root: RootId, path: &str) -> StdString {
    let mut out = format!("/{}", mount(root));
    let segments: StdVec<&str> = path.split('/').collect();
    let noise_at = tape.int_in(0usize..=segments.len(), "noise at");
    let noise = tape.int_in(0u8..=2, "noise");
    for (i, segment) in segments.iter().enumerate() {
        if i == noise_at {
            match noise {
                1 => out.push_str("/."),
                2 => out.push_str("/zz/.."),
                _ => {}
            }
        }
        out.push('/');
        out.push_str(segment);
    }
    out
}

/// One lookup drawn: a guest path and what it must come to.
fn draw_lookup(tape: &Tape) -> (StdString, Expect) {
    match tape.int_in(0u8..=9, "lookup kind") {
        0 => (StdString::from("/nowhere/x.h"), Expect::Unmounted),
        1 => (StdString::from("/package/../../etc/passwd"), Expect::Unmounted),
        2 => (StdString::from("/package/secret.txt"), Expect::Outside),
        3 => (StdString::from("/out/unit.o"), Expect::Outside),
        _ => {
            let (root, path, dir) = UNIVERSE[tape.index(UNIVERSE.len(), "entry")];
            let op = if dir || tape.coin("stat") { LookOp::Stat } else { LookOp::Read };
            (render(tape, root, path), Expect::At { root, path, op })
        }
    }
}

fn look_body(id: u32, op: LookOp, path: &str) -> StdVec<u8> {
    let mut body = vec![0x02];
    body.extend_from_slice(&id.to_be_bytes());
    body.push(op as u8);
    body.extend_from_slice(path.as_bytes());
    body
}

/// `event` told to `job` at time 1; a misuse is the harness's.
fn step<P: Planted<JobPlant>>(job: &mut Job<'_, '_, P>, event: Event<'_>) -> Result<(), Broken<JobBroken>> {
    job.step(1, event).map_err(|e| harness(&format!("{e:?}")))
}

fn args() -> blaze_own::AVec<'static, blaze_own::AString<'static>> {
    let mut args = Vec::new_in(heap());
    args.push(String::from_str_in("/shelf/clang/bin/clang", heap()));
    args.push(String::from_str_in("-fsyntax-only", heap()));
    args
}

/// A helper's run played against the machine, the view's files present by the tape's coins.
fn one_job(tape: &Tape, plant: impl Planted<JobPlant>) -> Result<(), Broken<JobBroken>> {
    with_roots(|roots, mounts| {
        let present: StdVec<bool> = UNIVERSE.iter().map(|e| e.2 || tape.coin("present")).collect();
        let mut job = Job::new(JobOpts {
            alloc: heap(),
            roots,
            mounts,
            mode: Mode::Decls,
            args: args(),
            limits: Limits::STANDARD,
            deadline: 1_000,
            confinement: Confinement::Seatbelt,
            plant,
        });
        if !matches!(job.next_effect(), Some(Effect::Spawn)) {
            return Err(harness("the first effect is not the spawn"));
        }
        step(&mut job, Event::Spawned)?;
        step(&mut job, Event::Message(&[0x01, 0, 2, 1]))?;
        match job.next_effect() {
            Some(Effect::Send(bytes)) => {
                let read = wire::read_job(&bytes.as_slice()[wire::PREFIX..], heap());
                let want: [&str; 2] = ["/shelf/clang/bin/clang", "-fsyntax-only"];
                if read.as_ref().map(|(m, a)| (*m, a.as_slice())) != Some((Mode::Decls, &want[..])) {
                    return Err(harness("the job message is not the job"));
                }
            }
            _ => return Err(harness("no job sent after the hello")),
        }
        let mut want_seen: StdVec<StdString> = StdVec::new();
        let count = tape.int_in(1usize..=12, "lookups");
        for id in 0..count as u32 {
            let (guest, expect) = draw_lookup(tape);
            let op = match expect {
                Expect::At { op, .. } => op,
                _ => LookOp::Read,
            };
            step(&mut job, Event::Message(&look_body(id, op, &guest)))?;
            let (root, path) = match expect {
                Expect::Unmounted | Expect::Outside => {
                    let refused = match job.next_effect() {
                        Some(Effect::Kill) => job.finish(),
                        other => return Err(Broken::Oracle(JobBroken::Refusal { guest, got: format!("{other:?}") })),
                    };
                    return match (expect, refused) {
                        (Expect::Unmounted, Err(JobError::Unmounted { why: Unmounted::Outside | Unmounted::Path(_), .. })) => Ok(()),
                        (Expect::Outside, Err(JobError::Outside { why: Outside::NotDeclared { .. } | Outside::OutputRead { .. }, .. })) => {
                            Ok(())
                        }
                        (_, got) => Err(Broken::Oracle(JobBroken::Refusal { guest, got: format!("{:?}", got.err()) })),
                    };
                }
                Expect::At { root, path, .. } => (root, path),
            };
            let (got_root, got_path) = match job.next_effect() {
                Some(Effect::Look { id: got, root, path, .. }) if got == id => (root, StdString::from(path.get().as_str())),
                Some(Effect::Kill) => {
                    let error = job.finish().err();
                    return Err(Broken::Oracle(JobBroken::Refusal { guest, got: format!("{error:?}") }));
                }
                other => return Err(harness(&format!("no lookup for {guest}: {other:?}"))),
            };
            if got_root != root || got_path != path {
                return Err(Broken::Oracle(JobBroken::Resolved {
                    guest,
                    want: format!("{root:?} {path}"),
                    got: format!("{got_root:?} {got_path}"),
                }));
            }
            let at = UNIVERSE.iter().position(|e| e.0 == root && e.1 == path).unwrap_or(0);
            let digest = [at as u8; 32];
            let (answer, seen) = match (present[at], UNIVERSE[at].2, op) {
                (false, _, _) => (Answer::Missing, Seen::Absent),
                (true, true, _) => (Answer::Dir, Seen::Dir),
                (true, false, LookOp::Read) => {
                    (Answer::File { digest, size: 2, bytes: Some(b"//") }, Seen::File { digest: Digest(digest), len: 2 })
                }
                (true, false, _) => (Answer::File { digest, size: 2, bytes: None }, Seen::File { digest: Digest(digest), len: 2 }),
            };
            want_seen.push(format!("{root:?} {path} {seen:?}"));
            step(&mut job, Event::Looked { id, answer })?;
            if !matches!(job.next_effect(), Some(Effect::Send(_))) {
                return Err(harness("a lookup's answer was not sent"));
            }
        }
        step(&mut job, Event::Message(b"\x03\x02cfront-decls 2\n"))?;
        step(&mut job, Event::Message(&[0x04, 0]))?;
        step(&mut job, Event::Exited(Ended::Exited(0)))?;
        let finished = job.finish().map_err(|e| harness(&format!("the job failed: {e:?}")))?;
        let mut got: StdVec<StdString> =
            finished.reads.entries().map(|e| format!("{:?} {} {:?}", e.root, e.path.as_str(), e.seen)).collect();
        want_seen.sort();
        want_seen.dedup();
        got.sort();
        if got != want_seen {
            return Err(Broken::Oracle(JobBroken::Reads { want: format!("{want_seen:?}"), got: format!("{got:?}") }));
        }
        Ok(())
    })
}

#[test]
fn every_lookup_is_resolved_refused_or_recorded() {
    check(Config::cases(96), |tape| one_job(tape, NoPlant));
}

#[test]
fn every_job_plant_is_caught_by_its_invariant() {
    controls(Config::cases(96), |tape, plant: JobPlant| one_job(tape, plant));
}

#[test]
fn a_helper_that_is_not_confined_or_speaks_another_version_is_refused() {
    with_roots(|roots, mounts| {
        let opts = |confinement| JobOpts {
            alloc: heap(),
            roots,
            mounts,
            mode: Mode::Compile,
            args: Vec::new_in(heap()),
            limits: Limits::STANDARD,
            deadline: 100,
            confinement,
            plant: NoPlant,
        };
        let begun = |confinement| {
            let mut job = Job::new(opts(confinement));
            job.next_effect();
            job.step(0, Event::Spawned).expect("spawned");
            job
        };
        let mut job = begun(Confinement::Seccomp);
        job.step(0, Event::Message(&[0x01, 0, 2, 0])).expect("a hello");
        assert!(matches!(job.next_effect(), Some(Effect::Kill)));
        assert_eq!(job.finish().err(), Some(JobError::Unconfined { want: Confinement::Seccomp, got: Confinement::None }));
        // Landlock with seccomp covers seccomp.
        let mut job = begun(Confinement::Seccomp);
        job.step(0, Event::Message(&[0x01, 0, 2, 3])).expect("a hello");
        assert_eq!(job.state(), Phase::Serving);
        let mut job = begun(Confinement::None);
        job.step(0, Event::Message(&[0x01, 0, 1, 0])).expect("a hello");
        assert_eq!(job.finish().err(), Some(JobError::Version { got: 1 }));
        // A crash before its end fails the job alone (rule 20), and the deadline is a refusal.
        let mut job = begun(Confinement::None);
        job.step(0, Event::Exited(Ended::Signaled(11))).expect("an exit");
        assert_eq!(job.finish().err(), Some(JobError::Crashed { ended: Ended::Signaled(11), phase: Phase::Greeting }));
        let mut job = begun(Confinement::None);
        job.step(100, Event::Waited).expect("time passed");
        assert!(matches!(job.next_effect(), Some(Effect::Kill)));
        assert_eq!(job.finish().err(), Some(JobError::Deadline { at: 100 }));
        // A compile that says it compiled and sent no object is refused.
        let mut job = begun(Confinement::None);
        job.step(0, Event::Message(&[0x01, 0, 2, 0])).expect("a hello");
        job.step(0, Event::Message(&[0x04, 0])).expect("done");
        job.step(0, Event::Exited(Ended::Exited(0))).expect("an exit");
        assert_eq!(job.finish().err(), Some(JobError::NoOutput { mode: Mode::Compile }));
        // Diagnostics are rows, their files roots and paths; a listing is checked and digested.
        let mut job = begun(Confinement::None);
        job.step(0, Event::Message(&[0x01, 0, 2, 0])).expect("a hello");
        job.step(0, Event::Message(b"\x05\x04\x00\x00\x00\x09\x00\x00\x00\x0c\x00\x14/package/include/a.hbad")).expect("a row");
        let first = job.diagnostics().first_error().map(|d| (d.code, d.offset, d.file.map(|(r, p)| (r, StdString::from(p.as_str())))));
        assert_eq!(first, Some((9, 12, Some((PACKAGE, StdString::from("include/a.h"))))));
        job.step(0, Event::Message(b"\x02\x00\x00\x00\x01\x03/package/include")).expect("a list");
        while !matches!(job.next_effect(), Some(Effect::Look { .. }) | None) {}
        job.step(0, Event::Looked { id: 1, answer: Answer::Listing { names: &["a.h", "sub/"] } }).expect("listed");
        assert!(job.error().is_none());
        // An unsorted listing is the driver's bug, refused by name.
        job.step(0, Event::Message(b"\x02\x00\x00\x00\x02\x03/package/include")).expect("a list");
        job.step(0, Event::Looked { id: 2, answer: Answer::Listing { names: &["sub/", "a.h"] } }).expect("listed");
        assert_eq!(job.error(), Some(&JobError::AnswerMismatch { id: 2 }));
    });
}

// The declarations.

const fn sc(size: u8, align: u8) -> Scalar {
    Scalar { size, align }
}

/// aarch64-apple-darwin's C.
const ABI: CAbi = CAbi {
    endian: Endian::Little,
    char_signed: true,
    pointer: sc(8, 8),
    bool_: sc(1, 1),
    short: sc(2, 2),
    int: sc(4, 4),
    long: sc(8, 8),
    long_long: sc(8, 8),
    int128: Some(sc(16, 16)),
    float: sc(4, 4),
    double: sc(8, 8),
    long_double: sc(8, 8),
    max_align: 8,
};

/// A hand-written document of the shapes RocksDB's `c.h` has, and a few it does not: every
/// line kind but `ns`, `ref` and `base` (C++), and the integer kinds' facts checked.
const SAMPLE: &str = "cfront-decls 2
abi 8 8 1 1
header 2f7061636b6167652f696e636c7564652f726f636b7364622f632e68
ty int 5 4 4 1 0
ty int 0 1 1 1 1
ty ptr 1 0
record 0 rocksdb_t - 0 4120 20
ty record 0 0
ty ptr 3 0
ty ptr 2 0
ty fn 4 0 1 0 2 5
fn rocksdb_open - rocksdb_open 6 0 - 0 0 0 0 5310 60 name errptr
ty void 0
ty fn 7 1 1 0 2
fn rocksdb_log - rocksdb_log 8 0 - 0 0 0 0 5400 40 fmt
fn rocksdb_fast - rocksdb_fast 8 0 - 0 1 1 0 130 50 fmt
ty int 8 8 8 0 0
typedef size_t - 9 0 10 20
record 0 point - 0 300 30
define-record 1 8 4 - 0
field x 0 0 - - 14 5
field y 0 32 - - 21 5
end
record 1 - - 0 330 40
define-record 2 8 8 - 0
unit 0 8
field bits 9 0 3 0 10 9
field all 9 0 - - 20 8
end
enum level - 0 400 30
define-enum 0 0 1 0
const low 0 13 3
const high 18446744073709551614 18 9
end
var rocksdb_version - rocksdb_version 2 0 0 absent 0 90 30
var rocksdb_table - rocksdb_table 2 0 1 zero 0 160 30
macro int ROCKSDB_MAJOR 0 10 0 20 23
macro slot ROCKSDB_SLOT 4 1 3 1 0 140 30
macro addr ROCKSDB_VERSION_PTR 0 0 150 30
macro str ROCKSDB_NAME 1 726f636b73 0 30 25
ty float 1 8 8 0
macro float ROCKSDB_PI 10 4614256656552045848 0 40 30
ty valist 2 0
unexpressed ROCKSDB_CHECK - 6 0 0 120 40
end
";

/// `doc` read in pieces cut at `cuts`, its model in the record's canonical bytes.
fn read(doc: &[u8], cuts: &[usize], limits: DeclsLimits, plant: impl Planted<DeclsPlant>) -> Result<(StdVec<u8>, Census), DeclsError> {
    with_roots(|_, mounts| {
        let into = DeclsBuilder::new(DeclsOpts { abi: abi_row(&ABI), alloc: heap() });
        let mut reader =
            DeclsReader::new(DeclsReaderOpts { into, abi: ABI, mounts, places: &PLACES, limits, alloc: heap(), plant });
        let mut from = 0;
        for &cut in cuts.iter().chain(core::iter::once(&doc.len())) {
            let cut = cut.clamp(from, doc.len());
            reader.feed(&doc[from..cut])?;
            from = cut;
        }
        let (model, census) = reader.finish()?;
        let bytes = blaze_c::encode(&model, heap(), NoPlant).expect("the builder took the model, so it encodes");
        Ok((StdVec::from(bytes.as_slice()), census))
    })
}

#[test]
fn the_sample_reads_as_written() {
    let (_, census) = read(SAMPLE.as_bytes(), &[], DeclsLimits::STANDARD, NoPlant).expect("the sample is well formed");
    assert_eq!(
        (census.headers, census.types, census.records, census.layouts, census.enums, census.typedefs),
        (1, 12, 3, 2, 1, 1)
    );
    assert_eq!((census.functions, census.variables, census.constants, census.left_out()), (3, 2, 5, 1));
    assert_eq!(census.unexpressed[0], 1);
}

#[test]
fn malformed_documents_are_refused_at_their_line() {
    let refused = |doc: &str| read(doc.as_bytes(), &[], DeclsLimits::STANDARD, NoPlant).err().map(|e| (e.line, e.why));
    let head = "cfront-decls 2\nabi 8 8 1 1\nheader 2f7061636b6167652f612e68\n";
    assert_eq!(refused("cfront-decls 1\n"), Some((1, DeclsWhy::Magic)));
    assert_eq!(refused("cfront-decls 2\nabi 4 4 1 1\n"), Some((2, DeclsWhy::Abi)));
    assert!(matches!(refused(&format!("{head}ty ptr 5 0\nend\n")), Some((4, DeclsWhy::Model(_)))));
    assert_eq!(refused(&format!("{head}ty int 5 8 8 1 0\nend\n")), Some((4, DeclsWhy::IntFacts { kind: IntKind::Int })));
    assert_eq!(refused(&format!("{head}ty int 0 1 1 0 0\nend\n")), Some((4, DeclsWhy::IntFacts { kind: IntKind::Char })));
    assert_eq!(refused(&format!("{head}ty int 05 4 4 1 0\nend\n")), Some((4, DeclsWhy::Token { what: "integer kind" })));
    assert_eq!(refused(&format!("{head}ty int 99 4 4 1 0\nend\n")), Some((4, DeclsWhy::Byte { what: "integer kind" })));
    assert_eq!(refused(&format!("{head}ty void\nend\n")), Some((4, DeclsWhy::Arity)));
    assert_eq!(
        refused(&format!("{head}record 0 a - 0 0 1\ndefine-record 0 4 4 - 0\nenum b - 0 2 1\nend\n")),
        Some((6, DeclsWhy::Unclosed))
    );
    assert_eq!(
        refused(&format!("{head}ty void 0\nrecord 0 a - 0 0 1\ndefine-record 0 4 4 - 0\nfield x 0 0 - - 0 1\nunit 0 4\nend\nend\n")),
        Some((8, DeclsWhy::UnknownLine))
    );
    assert_eq!(refused("cfront-decls 2\nabi 8 8 1 1\nheader 2f6e6f77686572652f612e68\n"), Some((3, DeclsWhy::HeaderPlace)));
    assert_eq!(refused(&format!("{head}ty void 0\n")), Some((4, DeclsWhy::NotEnded)));
    assert_eq!(refused(&format!("{head}end\nmore\n")), Some((5, DeclsWhy::AfterEnd)));
    let short = DeclsLimits { most_line: 8, ..DeclsLimits::STANDARD };
    assert_eq!(
        read(b"cfront-decls 2 and more", &[], short, NoPlant).err().map(|e| e.why),
        Some(DeclsWhy::LineOver { most: 8 })
    );
}

#[derive(Debug)]
enum SplitBroken {
    /// The document read otherwise when cut at these places.
    Differs { cuts: StdVec<usize> },
}

impl fmt::Display for SplitBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let SplitBroken::Differs { cuts } = self;
        write!(f, "the sample reads otherwise when cut at {cuts:?}")
    }
}

blaze_sim::plants! {
    impl DeclsPlant breaks SplitBroken {
        None,
        LineRestarts => SplitBroken::Differs { .. },
    }
}

fn every_split(tape: &Tape, plant: impl Planted<DeclsPlant>) -> Result<(), Broken<SplitBroken>> {
    let whole = read(SAMPLE.as_bytes(), &[], DeclsLimits::STANDARD, NoPlant).map_err(|e| Broken::Harness(format!("{e:?}")))?;
    let mut cuts: StdVec<usize> = (0..tape.int_in(1usize..=6, "cuts")).map(|_| tape.index(SAMPLE.len() + 1, "cut")).collect();
    cuts.sort();
    match read(SAMPLE.as_bytes(), &cuts, DeclsLimits::STANDARD, plant) {
        Ok(split) if split.0 == whole.0 => Ok(()),
        _ => Err(Broken::Oracle(SplitBroken::Differs { cuts })),
    }
}

#[test]
fn a_document_reads_the_same_however_it_is_split() {
    check(Config::cases(64), |tape| every_split(tape, NoPlant));
}

#[test]
fn every_decls_plant_is_caught_by_its_invariant() {
    controls(Config::cases(64), |tape, plant: DeclsPlant| every_split(tape, plant));
}
