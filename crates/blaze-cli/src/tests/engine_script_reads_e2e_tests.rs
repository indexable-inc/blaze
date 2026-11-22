//! A member build script's reads of the checkout, end to end through `engine-check` on a
//! simulated world (engine_script_reads.rs). The script is a test double
//! (engine_scripts.rs `ScriptRunner`) in place of the job and the wasm run: it reads
//! `stat:pkg`, `dir:pkg/data` and `file:pkg/data/in.txt`, each answered as the sandbox answers
//! (`engine_script_reads::now`), and prints `cargo::rustc-cfg=pkg_flag`, always (`Fixed`) or only
//! while `in.txt` holds `one` (`ByBytes`). Its record is made by the one maker of a script's
//! record (scripts.rs `outputs_record`). `pkg`'s `f` exists under that cfg; `app`'s `g` calls it.

use super::*;

use core::sync::atomic::{AtomicU32, Ordering};

use crate::engine_script_reads::{self as reads, ReadKind, ReadsPlant};
use crate::sweep::{Plant, Stamp, Stamps};

use super::engine_check_tests::engine_from;

/// No stamp proves anything (the double reads every answer, as the sandbox does).
struct NoStamps;

impl Stamps for NoStamps {
    fn stamp(&self, _: &str, _: &bstd::fs::Meta) -> Option<Stamp> {
        None
    }

    fn swept_at_ns(&self) -> u64 {
        0
    }

    fn granularity_ns(&self) -> u64 {
        1
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prints {
    /// `pkg_flag` whatever `in.txt` holds: an edit of it changes no output.
    Fixed,
    /// `pkg_flag` only while `in.txt` holds `one\n`.
    ByBytes,
}

const READS: [(ReadKind, &str); 3] = [(ReadKind::Stat, "pkg"), (ReadKind::Listing, "pkg/data"), (ReadKind::File, "pkg/data/in.txt")];

struct Double {
    runs: AtomicU32,
    prints: Prints,
    /// The bug planted in its read list, over the stage's (the CONTROL).
    plant: ReadsPlant,
    /// The cargo rerun lines it prints besides its cfg (one cargo case each; empty: none).
    declared: &'static [u8],
}

impl crate::engine_scripts::ScriptRunner for Double {
    fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<crate::engine_scripts::KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let frame = cx.frame();
        let mut recorded: std::vec::Vec<(std::string::String, Option<blaze_ids::ContentId>)> = std::vec::Vec::new();
        let mut flag = self.prints == Prints::Fixed;
        for (kind, path) in READS.iter() {
            let (_, _, answer) = reads::now(cx, root, &NoStamps, Plant::None, *kind, path, None, frame).expect("the double's read is answered");
            let word = match kind {
                ReadKind::File => "file",
                ReadKind::Listing => "dir",
                ReadKind::Stat => "stat",
                ReadKind::Git => "git",
            };
            recorded.push((format!("{word}:{path}"), (answer != reads::NONE).then_some(answer)));
            if *kind == ReadKind::File && answer == blaze_proc_macro::script_sandbox::file_answer(b"one\n") {
                flag = true;
            }
        }
        let pooled = |did: crate::scripts::Ran, checkout| crate::scripts::PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept: None };
        let Some((stage_plant, _)) = kept else {
            let mut words = blaze_own::Vec::new_in(std::alloc::Global);
            words.extend_from_slice(b"the double read the checkout on a path that keeps no reads");
            return pooled(crate::scripts::Ran::Failed { what: "a member's build script read a checkout file the drive cannot name yet", words, hole: true }, reads::ReadList::new_in(blaze_own::testing::heap().shared()));
        };
        let plant = if self.plant == ReadsPlant::None { stage_plant } else { self.plant };
        let cells = reads::cells_of(recorded.iter().map(|(subject, digest)| (subject.as_str(), *digest)), plant).expect("every read has a cell");
        let read_rows: std::vec::Vec<_> = cells.iter().map(|read| (read.name(), read.answer)).collect();
        let erased = blaze_own::Erased::new(std::alloc::Global);
        let mut printed: std::vec::Vec<u8> = std::vec::Vec::new();
        if flag {
            printed.extend_from_slice(b"cargo::rustc-cfg=pkg_flag\n");
        }
        printed.extend_from_slice(self.declared);
        let outputs = blaze_build_script::outputs::Outputs::read(&printed, "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle()).expect("the double's output reads");
        match crate::scripts::outputs_record(name, &read_rows[..], crate::scripts::RanAs::Wasm, &outputs, "pkg") {
            made @ crate::scripts::Ran::Made(_) => pooled(made, cells),
            failed => pooled(failed, reads::ReadList::new_in(blaze_own::testing::heap().shared())),
        }
    }
}

fn double(prints: Prints, plant: ReadsPlant) -> &'static Double {
    std::boxed::Box::leak(std::boxed::Box::new(Double { runs: AtomicU32::new(0), prints, plant, declared: b"" }))
}

/// [`double`] printing `declared` too: cargo's rerun lines, one case each.
fn declaring(prints: Prints, declared: &'static [u8]) -> &'static Double {
    std::boxed::Box::leak(std::boxed::Box::new(Double { runs: AtomicU32::new(0), prints, plant: ReadsPlant::None, declared }))
}

fn world(seed: u64, text: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"pkg\", \"app\"]\nresolver = \"2\"\n");
    write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
    write(&io, "pkg/build.rs", "fn main() {\n    println!(\"cargo::rustc-cfg=pkg_flag\");\n}\n");
    write(&io, "pkg/src/lib.rs", "#[cfg(pkg_flag)]\npub fn f() -> u8 {\n    1\n}\n");
    write(&io, "pkg/data/in.txt", text);
    write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\npkg = { path = \"../pkg\" }\n");
    write(&io, "app/src/lib.rs", "pub fn g() -> u8 {\n    pkg::f()\n}\n");
    io
}

type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

/// One `engine-check` request from the stored answer, with `runner` for the scripts.
fn request(io: &Io, runner: &'static Double) -> (Ids, engine_check::Checked, std::string::String) {
    let before = stdout(io).len();
    let (ids, checked) = crate::engine_scripts::with_runner(runner, || engine_from(io, 1, false));
    (ids, checked, stdout(io).split_off(before))
}

fn entered(checked: &engine_check::Checked, stage: blaze_engine::Stage) -> u32 {
    checked.stages[stage.index()].entered
}

/// Whether a group of the scripts stage was entered because the cell `of` moved.
fn entered_by(checked: &engine_check::Checked, of: blaze_drive::Name) -> bool {
    checked.because.iter().any(|(group, _)| group.stage == blaze_engine::Stage::Scripts && group.definer == Some(of))
}

/// (a) the first request keeps the run's outputs: no "cannot name yet" note, the script ran
/// once, `app`'s body is checked and nothing is refused; (b) the second is certified, enters
/// nothing and runs no script.
#[test]
fn a_member_script_s_reads_keep_its_outputs_and_certify() {
    let io = world(31, "one\n");
    let runner = double(Prints::Fixed, ReadsPlant::None);
    let (ids, checked, said) = request(&io, runner);
    assert!(!said.contains("cannot name yet"), "no hole for the script's reads: {said}");
    assert_eq!(runner.runs.load(Ordering::SeqCst), 1, "the script ran once");
    assert!(ids.is_empty(), "`app`'s call resolves under the script's cfg: {ids:?}\n{said}");
    assert!(checked.bodies.1 >= 1, "`app`'s body is checked: {:?}", checked.bodies.0);
    assert!(entered(&checked, blaze_engine::Stage::Scripts) >= 1, "the script and its reads' cells were entered");
    let (_, again, said) = request(&io, runner);
    assert!(again.certified, "nothing moved: the certificate stands\n{said}");
    assert_eq!(again.entered, 0, "no task is entered");
    assert_eq!(runner.runs.load(Ordering::SeqCst), 1, "the script is not run again");
}

/// (c) an edit of `in.txt` enters the script, named by the file's cell; `app`'s body is entered
/// only when the outputs changed: not under `Fixed`, and under `ByBytes`, where `f` goes.
#[test]
fn an_edit_of_a_read_file_enters_the_script_and_only_what_its_outputs_move() {
    for prints in [Prints::Fixed, Prints::ByBytes] {
        let io = world(32, "one\n");
        let runner = double(prints, ReadsPlant::None);
        let (ids, ..) = request(&io, runner);
        assert!(ids.is_empty(), "{prints:?}: the first answer is clean: {ids:?}");
        write(&io, "pkg/data/in.txt", "two\n");
        let (ids, checked, said) = request(&io, runner);
        assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "{prints:?}: the edit enters the script once\n{said}");
        assert!(entered_by(&checked, reads::cell_name(ReadKind::File, "pkg/data/in.txt")), "{prints:?}: the cause names the file's cell: {:?}", checked.because.iter().map(|(group, count)| (group.stage, group.cause, group.definer, *count)).collect::<std::vec::Vec<_>>());
        let bodies = entered(&checked, blaze_engine::Stage::Bodies);
        match prints {
            Prints::Fixed => {
                assert_eq!(bodies, 0, "the outputs did not change: no body is entered\n{said}");
                assert!(ids.is_empty(), "the answer stands: {ids:?}");
            }
            Prints::ByBytes => {
                assert!(bodies >= 1, "the cfg went: `app`'s body is entered\n{said}");
                assert!(!ids.is_empty(), "`pkg::f` no longer exists, so `app`'s call is refused\n{said}");
            }
        }
    }
}

/// (d) a file added under `pkg/data` enters the script (the listing moved); one added beside it
/// enters nothing, and the request is certified.
#[test]
fn a_file_added_under_the_listed_folder_enters_the_script_and_beside_it_nothing() {
    let io = world(33, "one\n");
    let runner = double(Prints::Fixed, ReadsPlant::None);
    request(&io, runner);
    write(&io, "pkg/data/new.txt", "x\n");
    let (_, checked, said) = request(&io, runner);
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "the listing moved: the script runs again\n{said}");
    assert!(entered_by(&checked, reads::cell_name(ReadKind::Listing, "pkg/data")), "the cause names the listing's cell");
    write(&io, "pkg/beside.txt", "x\n");
    let (_, checked, said) = request(&io, runner);
    assert!(checked.certified, "a file no script read: certified\n{said}");
    assert_eq!(checked.entered, 0, "nothing is entered");
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "the script is not run");
}

/// (e) CONTROL: with the `file:` read dropped, the edit of (c) is not seen: the script is not run
/// and the answer keeps `f`, while a fresh world holding the edit refuses `app`'s call.
#[test]
fn control_a_dropped_file_read_keeps_a_stale_answer() {
    let io = world(34, "one\n");
    let planted = double(Prints::ByBytes, ReadsPlant::FileDropped);
    request(&io, planted);
    write(&io, "pkg/data/in.txt", "two\n");
    let (stale, _, said) = request(&io, planted);
    assert_eq!(planted.runs.load(Ordering::SeqCst), 1, "the planted read list misses the edit\n{said}");
    let fresh_world = world(34, "two\n");
    let (fresh, ..) = request(&fresh_world, double(Prints::ByBytes, ReadsPlant::None));
    assert_ne!(stale, fresh, "the planted answer differs from a fresh world's");
    assert!(stale.is_empty() && !fresh.is_empty(), "the stale answer keeps `f`; the fresh one refuses the call: {stale:?} against {fresh:?}");
}

/// The profile's scripts counters line (engine_scripts.rs `ScriptsStage::profile`).
fn script_counters(io: &Io) -> std::string::String {
    super::engine_check_tests::profile(io).lines().filter(|line| line.contains("Scripts: given cells moved") || line.contains("Scripts: identical again")).collect::<std::vec::Vec<_>>().join("\n")
}

/// (f) A BODY EDIT IN ANOTHER CRATE ENTERS NO SCRIPT (the floor: 0 cells moved, 0 scripts
/// entered): `app`'s body moves, no read of `pkg`'s script does, so no `Given` cell moves, the
/// script is not run and the scripts stage enters nothing; the answer is a fresh world's.
/// CONTROL: an edit of the file the script reads enters it, and the profile names the run as
/// identical again (its outputs are `Fixed`), woken by that file's cell.
#[test]
fn a_body_edit_in_another_crate_enters_no_script() {
    let io = world(35, "one\n");
    let runner = double(Prints::Fixed, ReadsPlant::None);
    request(&io, runner);
    write(&io, "app/src/lib.rs", "pub fn g() -> u8 {\n    pkg::f() + 1\n}\n");
    let (ids, checked, said) = request(&io, runner);
    assert_eq!(runner.runs.load(Ordering::SeqCst), 1, "a body edit of `app` runs no script\n{said}");
    assert_eq!(entered(&checked, blaze_engine::Stage::Scripts), 0, "the scripts stage enters nothing\n{}", script_counters(&io));
    assert!(script_counters(&io).contains("given cells moved 0; scripts entered again 0; identical again 0"), "the counters at their floor:\n{}", script_counters(&io));
    let fresh_world = world(35, "one\n");
    write(&fresh_world, "app/src/lib.rs", "pub fn g() -> u8 {\n    pkg::f() + 1\n}\n");
    let (fresh, ..) = request(&fresh_world, double(Prints::Fixed, ReadsPlant::None));
    assert_eq!(ids, fresh, "the answer is a fresh world's");
    // CONTROL: the script's own read moved.
    write(&io, "pkg/data/in.txt", "two\n");
    let (_, _, said) = request(&io, runner);
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "the read file moved: the script runs\n{said}");
    let counters = script_counters(&io);
    assert!(counters.contains("scripts entered again 1; identical again 1"), "the rerun made the same bytes and is counted:\n{counters}");
    assert!(counters.contains("woken by") && counters.contains("pkg/data/in.txt"), "the run is named with the cell that woke it:\n{counters}");
}

/// (g) THE THREE CARGO CASES, ONE RULE: the real reads win (the sandbox's, engine_script_reads.rs),
/// and a script's `rerun-if-changed` and `rerun-if-env-changed` lines are dropped
/// (blaze-build-script outputs.rs), so its record is the same in every case. For each case: an
/// edit of the file the script READ enters it (where cargo, under `rerun-if-changed=other.txt`,
/// would miss it), and an edit of the file it only DECLARED enters nothing (where cargo would
/// rerun). The env case has no host variable to move: a script's environment is keyed whole
/// (blaze-build-script env.rs), so it moves only with the script's key.
#[test]
fn the_real_reads_decide_in_every_cargo_case() {
    let cases: [(&str, &'static [u8]); 3] = [
        ("none declared", b""),
        ("rerun-if-changed", b"cargo::rerun-if-changed=other.txt\n"),
        ("rerun-if-env-changed", b"cargo::rerun-if-env-changed=PKG_KNOB\n"),
    ];
    let mut answers = std::vec::Vec::new();
    for (case, declared) in cases {
        let io = world(36, "one\n");
        write(&io, "pkg/other.txt", "a\n");
        let runner = declaring(Prints::Fixed, declared);
        let (first, ..) = request(&io, runner);
        answers.push(first);
        write(&io, "pkg/other.txt", "b\n");
        let (_, checked, said) = request(&io, runner);
        assert_eq!(runner.runs.load(Ordering::SeqCst), 1, "{case}: a file the script never read enters nothing\n{said}");
        assert!(checked.certified, "{case}: certified\n{said}");
        write(&io, "pkg/data/in.txt", "two\n");
        let (_, _, said) = request(&io, runner);
        assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "{case}: the file it read enters it\n{said}");
    }
    assert!(answers.windows(2).all(|two| two[0] == two[1]), "the declared lines change no answer: {answers:?}");
}

mod parallel {
    use super::*;
    use blaze_build_script::env::{DepMetadata, Given, PackageFacts, env};
    use blaze_core::lock::Lock;
    use blaze_drive::{Kind, Name};
    use crate::engine_scripts::{ScriptClock, ScriptRunner};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Enter(Name, u64),
        Start(Name, u64),
        End(Name, u64),
    }

    struct Fixture {
        a: Name,
        b: Name,
        omit_edge: bool,
        events: Lock<std::vec::Vec<Event>>,
        missing: AtomicU32,
    }

    fn name(package: &str) -> Name {
        Name::new(Kind::Script, blaze_world::script_subject(package, "0.1.0", blaze_world::Side::Target, &[], blaze_own::testing::heap().alloc()))
    }

    impl ScriptClock for Fixture {
        fn start(&self, name: Name, tick: u64) -> u64 {
            self.events.lock().push(Event::Start(name, tick));
            tick + if name == self.a || name == self.b { 1 } else { 10 }
        }

        fn end(&self, name: Name, tick: u64) {
            self.events.lock().push(Event::End(name, tick));
        }

        fn omit_links_edge(&self, producer: Name, reader: Name) -> bool {
            self.omit_edge && producer == self.a && reader == self.b
        }
    }

    impl ScriptRunner for Fixture {
        fn clock(&self) -> Option<&dyn ScriptClock> { Some(self) }

        fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: Name, kept: Option<crate::engine_scripts::KeptReads>, producers: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
            let erased = blaze_own::Erased::new(cx.frame());
            let mut checkout = reads::ReadList::new_in(blaze_own::testing::heap().shared());
            if name == self.a || name == self.b {
                let (path, subject) = if name == self.a { ("a/input", "file:a/input") } else { ("b/input", "file:b/input") };
                let (_, _, answer) = reads::now(cx, root, &NoStamps, Plant::None, ReadKind::File, path, None, cx.frame()).unwrap();
                if name == self.a && answer == blaze_proc_macro::script_sandbox::file_answer(b"fail") {
                    let failed = crate::scripts::failed("a build script failed", format_args!("fixture producer a refused its input"), true);
                    return crate::scripts::PooledScript { did: Ok(failed), ns: 0, used: 0, checkout, unkept: None };
                }
                checkout = reads::cells_of([(subject, Some(answer))].into_iter(), kept.unwrap().0).unwrap();
            }
            let printed: &[u8] = if name == self.a {
                b"cargo::metadata=answer=42\n"
            } else if name == self.b {
                let mut deps = std::vec::Vec::new();
                for (_, bytes) in producers.iter().filter(|(input, _)| input.name == self.a) {
                    let (_, bytes) = crate::scripts::payload_parts(bytes).expect("a handed its Script record to b");
                    let record = blaze_build_script::outputs::OutputsRef::open(bytes).unwrap();
                    deps.extend(record.metadata().map(|(key, value)| DepMetadata { links: "a", key, value }));
                }
                let given = Given {
                    package: PackageFacts { name: "b", version: "0.1.0", ..PackageFacts::default() },
                    seen: blaze_expand::seen::SeenRoot::Member { dir: "b" },
                    features: &[], target_cfg: &[], host: crate::graph::TRIPLE, target: crate::graph::TRIPLE, deps: &deps,
                };
                if env(&given, erased.handle()).get("DEP_A_ANSWER") == Some("42") {
                    b"cargo::rustc-cfg=received_42\n"
                } else {
                    self.missing.fetch_add(1, Ordering::SeqCst);
                    b""
                }
            } else {
                b""
            };
            let outputs = blaze_build_script::outputs::Outputs::read(printed, "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle()).unwrap();
            let mut read_list: std::vec::Vec<_> = checkout.iter().map(|read| (read.name(), read.answer)).collect();
            read_list.extend(producers.iter().map(|(input, _)| (input.name, input.out)));
            let did = crate::scripts::outputs_record(name, &read_list, crate::scripts::RanAs::Wasm, &outputs, "fixture");
            // Two distinguishable notes make their relative printed order observable.
            let unkept = (name != self.a && name != self.b).then(|| reads::NoCell::Kind {
                subject: reads::Subject::from_str_in(&format!("fixture-{name}"), blaze_own::testing::heap().shared()),
            });
            crate::scripts::PooledScript { did: Ok(did), ns: 0, used: 0, checkout, unkept }
        }
    }

    fn workspace() -> Io {
        let mut world = SimWorld::new(35, Limits::USUAL);
        world.faults.max_latency = core::time::Duration::ZERO;
        let io = Io::simulated(world);
        write(&io, "a/input", "first");
        write(&io, "b/input", "first");
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\", \"d\"]\nresolver = \"2\"\n");
        for package in ["a", "b", "c", "d"] {
            let mut manifest = format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
            if package == "a" {
                manifest.push_str("links = \"a\"\n");
            }
            if package == "b" {
                manifest.push_str("[dependencies]\na = { path = \"../a\" }\n");
            }
            write(&io, &format!("{package}/Cargo.toml"), &manifest);
            write(&io, &format!("{package}/src/lib.rs"), "pub fn value() -> u8 { 1 }\n");
            let script = match package {
                "a" => "fn main() { println!(\"cargo::metadata=answer=42\"); }\n",
                "b" => "fn main() { assert_eq!(std::env::var(\"DEP_A_ANSWER\").unwrap(), \"42\"); println!(\"cargo::rustc-cfg=received_42\"); }\n",
                _ => "fn main() {}\n",
            };
            write(&io, &format!("{package}/build.rs"), script);
        }
        io
    }

    fn fixture(omit_edge: bool) -> &'static Fixture {
        std::boxed::Box::leak(std::boxed::Box::new(Fixture {
            a: name("a"), b: name("b"), omit_edge, events: Lock::new("script fixture events", std::vec::Vec::new()), missing: AtomicU32::new(0),
        }))
    }

    fn check(workers: u32, omit_edge: bool) -> (&'static Fixture, blaze_ids::ContentId, std::string::String) {
        let io = workspace();
        let fixture = fixture(omit_edge);
        let (_, checked) = crate::engine_scripts::with_runner(fixture, || engine_from(&io, workers, false));
        (fixture, checked.records.digest(), stdout(&io))
    }

    fn times(events: &[Event], task: Name) -> (u64, u64) {
        let mut starts = events.iter().filter_map(|event| match *event { Event::Start(n, t) if n == task => Some(t), _ => None });
        let mut ends = events.iter().filter_map(|event| match *event { Event::End(n, t) if n == task => Some(t), _ => None });
        let (Some(start), Some(end)) = (starts.next(), ends.next()) else { panic!("missing start or end for {task}") };
        assert_eq!((starts.next(), ends.next()), (None, None), "{task} must start and finish exactly once");
        (start, end)
    }

    #[test]
    fn ready_scripts_overlap_and_publish_the_serial_records_and_lines() {
        let (serial, records, lines) = check(1, false);
        let (parallel, other_records, other_lines) = check(3, false);
        assert_eq!(serial.missing.load(Ordering::SeqCst), 0);
        assert_eq!(parallel.missing.load(Ordering::SeqCst), 0, "b received DEP_A_ANSWER=42");
        let events = parallel.events.lock();
        assert_eq!(events.len(), 8);
        assert_eq!(times(&events, name("a")), (0, 1));
        assert_eq!(times(&events, name("b")), (1, 2), "b starts when a ends while c and d still run");
        for slow in ["c", "d"] {
            assert_eq!(times(&events, name(slow)), (0, 10));
        }
        let serial_events = serial.events.lock();
        for pair in serial_events.chunks_exact(2) {
            assert!(matches!(pair, [Event::Start(x, _), Event::End(y, _)] if x == y));
        }
        assert_eq!(records, other_records, "every record put, in put order");
        assert!(lines.contains(&format!("fixture-{}", name("c"))) && lines.contains(&format!("fixture-{}", name("d"))));
        assert_eq!(lines, other_lines, "all printed bytes, including both script notes");
        println!("a,c,d start at 0; b reads DEP_A_ANSWER=42 at 1; records and lines match");
    }

    #[test]
    fn control_missing_links_edge_starts_b_before_a_ends() {
        // Same manifests and dependency. The planted planner drops only its metadata edge.
        let (fixture, _, _) = check(4, true);
        let events = fixture.events.lock();
        assert!(times(&events, fixture.b).0 < times(&events, fixture.a).1);
        assert_eq!(fixture.missing.load(Ordering::SeqCst), 1);
        println!("caught b starting before a ended with DEP_A_ANSWER missing");
    }

    #[test]
    fn a_links_reader_stands_on_equal_output_and_resumes_after_producer_recovery() {
        let io = workspace();
        let fixture = fixture(false);
        let request = || {
            fixture.events.lock().clear();
            let before = stdout(&io).len();
            crate::engine_scripts::with_runner(fixture, || engine_from(&io, 3, false));
            (fixture.events.lock().clone(), stdout(&io).split_off(before))
        };
        request();
        write(&io, "b/input", "changed");
        let (stored, _) = request();
        assert!(!stored.iter().any(|e| matches!(e, Event::Start(n, _) if *n == fixture.a)), "a's record stands");
        assert_eq!(times(&stored, fixture.b), (0, 1), "b can read the stored a while c and d run");
        write(&io, "a/input", "second");
        let (equal, _) = request();
        assert_eq!(times(&equal, fixture.a), (0, 1));
        assert!(!equal.iter().any(|e| matches!(e, Event::Start(n, _) if *n == fixture.b)), "a returned equal metadata");
        write(&io, "a/input", "fail");
        let (failed, lines) = request();
        assert!(lines.contains("fixture producer a refused its input"), "{lines}");
        assert!(!failed.iter().any(|e| matches!(e, Event::Start(n, _) if *n == fixture.b)), "b cannot run with failed metadata");
        for slow in ["c", "d"] {
            assert_eq!(times(&failed, name(slow)), (0, 10), "unrelated work still finishes");
        }
        write(&io, "a/input", "recovered");
        let (recovered, _) = request();
        assert_eq!(times(&recovered, fixture.a), (0, 1), "a retries its owed failure on the next request");
        assert_eq!(times(&recovered, fixture.b), (1, 2), "b observed a's transition from absent to present");
        assert_eq!(fixture.missing.load(Ordering::SeqCst), 0);
        println!("b read stored a at 0; equal metadata skipped b; failed a blocked b; recovered a released b at 1");
    }

    fn ten_script_free(seed: u64) -> Io {
        let mut world = SimWorld::new(seed, Limits::USUAL);
        world.faults.max_latency = core::time::Duration::ZERO;
        let io = Io::simulated(world);
        let mut members = std::string::String::from("[workspace]\nmembers = [\"pkg\"");
        for at in 0..10 {
            members.push_str(&format!(", \"free{at}\""));
        }
        members.push_str("]\nresolver = \"2\"\n");
        write(&io, "Cargo.toml", &members);
        write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
        write(&io, "pkg/build.rs", "fn main() { println!(\"cargo::rustc-cfg=pkg_flag\"); }\n");
        write(&io, "pkg/data/in.txt", "one\n");
        write(&io, "pkg/src/lib.rs", "#[cfg(pkg_flag)] pub fn f() {}\npub fn needs_cfg() { f(); }\n");
        for at in 0..10 {
            let package = format!("free{at}");
            write(&io, &format!("{package}/Cargo.toml"), &format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"));
            write(&io, &format!("{package}/src/lib.rs"), "pub fn free() {}\n");
        }
        io
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Order { Producers, GlobalBarrier, MissingProducer }

    struct Timeline {
        now: u64,
        events: std::vec::Vec<Event>,
        crates: std::collections::BTreeMap<Name, std::string::String>,
    }

    impl Timeline {
        fn member_scopes(&self, krate: &str) -> [Name; 2] {
            let mut scopes = self.crates.iter().filter(|(_, label)| label.as_str() == krate).map(|(&task, _)| task);
            let (Some(first), Some(second), None) = (scopes.next(), scopes.next(), scopes.next()) else {
                panic!("{krate} must enter one library and one test harness");
            };
            [first, second]
        }

        fn free_scopes(&self) -> impl Iterator<Item = Name> + '_ {
            (0..10).flat_map(move |at| self.member_scopes(&format!("free{at}")))
        }
    }

    struct SlowScript {
        outputs: Double,
        order: Order,
        // The simulation permutes the ten costs before the request. The worker queues
        // choose which result to deliver using its due tick, just as for the links fixture.
        costs: [u64; 10],
        clock: Lock<Timeline>,
    }

    impl ScriptClock for SlowScript {
        fn now(&self) -> u64 { self.clock.lock().now }

        fn start(&self, script: Name, tick: u64) -> u64 {
            assert_eq!(script, name("pkg"), "this world has one script");
            let mut clock = self.clock.lock();
            let start = tick.max(clock.now);
            clock.events.push(Event::Start(script, start));
            start + 1000
        }

        fn end(&self, task: Name, tick: u64) {
            let mut clock = self.clock.lock();
            assert!(tick >= clock.now, "the simulated pool delivered {task} at {tick} after {}", clock.now);
            clock.now = tick;
            clock.events.push(Event::End(task, tick));
        }

        fn names_entered(&self, task: Name, krate: &str) {
            let mut clock = self.clock.lock();
            let tick = clock.now;
            clock.events.push(Event::Enter(task, tick));
            assert!(clock.crates.insert(task, krate.to_owned()).is_none(), "{krate} task {task} entered twice");
        }

        fn names_admitted(&self, task: Name, krate: &str) -> u64 {
            let mut clock = self.clock.lock();
            let start = clock.now;
            clock.events.push(Event::Start(task, start));
            let cost = krate.strip_prefix("free").map(|at| self.costs[at.parse::<usize>().unwrap()]).unwrap_or(1);
            start + cost
        }

        fn names_finished(&self, task: Name, tick: u64) { self.end(task, tick); }

        fn global_names_barrier(&self) -> bool { self.order == Order::GlobalBarrier }

        fn drop_names_producer(&self) -> bool { self.order == Order::MissingProducer }
    }

    impl ScriptRunner for SlowScript {
        fn clock(&self) -> Option<&dyn ScriptClock> { Some(self) }

        fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, task: Name, kept: Option<crate::engine_scripts::KeptReads>, producers: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
            self.outputs.run(cx, root, task, kept, producers)
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct AnswerBytes {
        // Covers keys, lengths and bytes of all records in publication order.
        records: blaze_ids::ContentId,
        // Includes the actual wire bytes, headers, extra records and fine-grained reads.
        names: std::vec::Vec<crate::engine_names::schedule_test::Row>,
        printed: std::string::String,
    }

    fn overlap_request(seed: u64, workers: u32, order: Order) -> (&'static SlowScript, Result<AnswerBytes, CliError>) {
        let io = ten_script_free(seed);
        let mut costs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        io.with_sim(|sim| sim.rng().shuffle(&mut costs)).expect("a simulation");
        let runner = std::boxed::Box::leak(std::boxed::Box::new(SlowScript {
            outputs: Double { runs: AtomicU32::new(0), prints: Prints::Fixed, plant: ReadsPlant::None, declared: b"" },
            order,
            costs,
            clock: Lock::new("scripts and names simulation", Timeline { now: 0, events: std::vec::Vec::new(), crates: std::collections::BTreeMap::new() }),
        }));
        let frame = Frame::with_reserve(engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a request frame");
        let opts = engine_check::Opts { frame: &frame, ..super::super::engine_check_tests::opts(&io, workers, false) };
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().unwrap().open_dir(&cx, &Path::new("ws").unwrap()).unwrap();
        let out = io.need_out().unwrap();
        let err = io.need_err().unwrap();
        let mut phases = Phases::new(io.need_clock().unwrap());
        let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", cx.frame());
        let fs = io.need_fs().unwrap();
        let host = io.need_host().unwrap();
        let mut rng = io.need_rng().unwrap();
        let args = io.args();
        let net = io.net();
        let (answer, mut names) = crate::engine_names::schedule_test::capture(false, false, || {
            crate::engine_scripts::with_runner(runner, || engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts, &mut shown, &mut phases, &out, &err))
        });
        // Compare the serialized value for every task by identity, including dependent pkg.
        names.sort_unstable_by_key(|row| row.name);
        let answer = answer.map(|checked| {
            let printed = stdout(&io);
            assert!(shown.listed.is_empty(), "pkg::needs_cfg needs the function enabled by its script\n{printed}");
            let clock = runner.clock.lock();
            assert_eq!(names.len(), clock.crates.len(), "one published Names row per entered task");
            for (&task, row) in clock.crates.keys().zip(&names) {
                assert_eq!(row.name, task, "publication and entry must name the same task");
                assert!(!row.wire.is_empty(), "the Names record of {task} includes its bytes");
                times(&clock.events, task);
            }
            AnswerBytes { records: checked.records.digest(), names, printed }
        });
        (runner, answer)
    }

    /// Uses completion events emitted only after the scheduler takes a worker's result.
    /// There must be exactly one of each start and end; an admitted but unfinished task fails.
    fn free_names_late(clock: &Timeline) -> std::vec::Vec<Name> {
        let (_, script_end) = times(&clock.events, name("pkg"));
        clock.free_scopes().filter(|&task| times(&clock.events, task).1 >= script_end).collect()
    }

    #[test]
    fn script_free_names_finish_before_the_slow_script_and_shuffle_keeps_bytes() {
        let (_, serial) = overlap_request(100, 1, Order::Producers);
        let serial = serial.expect("the serial answer");
        let mut deliveries = std::collections::BTreeSet::new();
        for seed in 0..100 {
            let (runner, answer) = overlap_request(seed + 101, 4, Order::Producers);
            assert_eq!(answer.expect("the shuffled answer"), serial, "seed {seed}");
            let clock = runner.clock.lock();
            let (script_start, script_end) = times(&clock.events, name("pkg"));
            assert!(free_names_late(&clock).is_empty(), "seed {seed}: {:?}", clock.events);
            assert!(clock.free_scopes().any(|task| times(&clock.events, task).1 > script_start),
                "seed {seed}: the script and the free names must run beside each other");
            for reader in clock.member_scopes("pkg") {
                assert!(clock.events.iter().any(|e| matches!(*e, Event::Enter(task, tick) if task == reader && tick >= script_end)),
                    "seed {seed}: pkg task {reader} entered before its cfg existed");
            }
            deliveries.insert(clock.events.iter().filter_map(|event| match *event {
                Event::End(task, _) if task.kind == Kind::CrateScope => Some(task),
                _ => None,
            }).collect::<std::vec::Vec<_>>());
        }
        assert!(deliveries.len() > 1, "the 100 seeds must actually change the order completed results arrive");
        println!("ten script-free names finish before the slow script; 100 shuffled completions keep the serial bytes");
    }

    #[test]
    fn control_a_global_script_barrier_delays_all_ten_free_names() {
        let (runner, answer) = overlap_request(101, 4, Order::GlobalBarrier);
        answer.expect("the serial ordering still computes the right cfg");
        let clock = runner.clock.lock();
        assert_eq!(free_names_late(&clock), clock.free_scopes().collect::<std::vec::Vec<_>>());
        println!("caught all ten script-free names finishing after the slow script");
    }

    #[test]
    fn control_early_names_admission_reads_a_missing_script_cfg() {
        let (runner, answer) = overlap_request(101, 4, Order::MissingProducer);
        let clock = runner.clock.lock();
        let producer = name("pkg");
        let Err(CliError::EngineCheck(engine_check::EngineCheckError::Unseen { task: reader, read })) = answer else {
            panic!("the prematurely entered pkg must name its absent script input: {answer:?}");
        };
        assert_eq!(read, producer);
        assert_eq!(clock.crates[&reader], "pkg");
        let entered = clock.events.iter().position(|e| matches!(*e, Event::Enter(task, _) if task == reader)).unwrap();
        assert!(!clock.events[..entered].iter().any(|e| matches!(*e, Event::End(task, _) if task == producer)));
        println!("caught pkg entering before its script published pkg_flag");
    }

    #[test]
    fn stage_wall_spans_gaps_and_overlap_counts_only_jointly_busy_time() {
        use blaze_engine::{Phase, Stage};
        use crate::engine_times::Times;

        const SEC: u64 = 1_000_000_000;
        let mut times = Times::new_in(blaze_own::testing::heap().shared());
        times.interval(name("a"), 3 * SEC, 5 * SEC);
        times.interval(name("b"), 0, 2 * SEC);
        times.interval(name("c"), 4 * SEC, 6 * SEC);
        let scripts = times.fold(Phase::new(Stage::Scripts, 0), 0, 10 * SEC, 0, 0);
        times.interval(name("d"), SEC, 3 * SEC);
        times.interval(name("e"), 2 * SEC, 5 * SEC);
        let first = times.fold(Phase::new(Stage::Names, 0), 0, 10 * SEC, 0, 0);
        times.interval(name("f"), 8 * SEC, 9 * SEC);
        let last = times.fold(Phase::new(Stage::Names, 1), 0, 10 * SEC, 0, 0);
        let mut phases = [scripts, first, last];
        let accounts = times.finish(&mut phases, core::num::NonZeroU32::MIN);
        let mut text = std::string::String::new();
        crate::engine_times::write(&phases, &accounts, core::num::NonZeroU32::MIN, &|_, _| false, &mut text);
        let wall = |name: &str| {
            text.lines().find(|line| line.trim_start().starts_with(name)).unwrap().split_whitespace().nth(2).unwrap().parse::<f64>().unwrap()
        };
        assert_eq!(wall("Scripts:"), 6000.0, "first script start to last script end\n{text}");
        assert_eq!(wall("Names:"), 8000.0, "first names start to last names end, including gaps\n{text}");
        assert!(text.lines().any(|line| line == "Names ran 3.0 s beside Scripts"), "jointly busy in [1,2) and [3,5)\n{text}");
        println!("Scripts wall 6.0 s, Names wall 8.0 s, Names ran 3.0 s beside Scripts");
    }
}

/// THE STATUS AFTER A CHANGE, END TO END (engine_script_reads.rs `CheckoutSweep`, blaze-build-script
/// git.rs `status_after`): three commands on one checkout, journaled as FSEvents journals it.
/// Two member scripts (`pkg`'s, `app`'s) ask `status --porcelain --untracked-files=no` of a
/// served checkout whose index tracks `f` (engine_script_reads_tests.rs `status_files`), the
/// same bytes in the served tree and in the world the journal watches.
///
/// 1. A clean check: both scripts run; the status is computed whole (no previous answer) and
///    its answer and coverage kept under `<answer root>.status`.
/// 2. `f` and `app`'s body edited: the status is answered from the kept answer and the journal's
///    names (git `Made::Incremental`, the profile's `sweep: status:` line), it moved (` M f`),
///    and both scripts run.
/// 3. The same edited again: the status text is unchanged, no script runs, and the status
///    listed no folder and read no ignore file (the floor: 0, 0).
///
/// THE CONTROL (rule 23): the coverage written and never named by the root
/// ([`CoveragePlant::Dropped`]): the next command has no previous answer, says so
/// (`NoPrevious`), and the oracle refuses it.
mod status_after {
    use super::*;

    use core::sync::atomic::AtomicU8;

    use crate::engine_script_reads::CoveragePlant;

    /// The question both scripts record, as a `git:` subject (its folder the worktree's root).
    const ASKED: &str = "git:\nstatus --porcelain --untracked-files=no";
    /// `f`'s bytes by state: the index holds `a\n`.
    const F: [&str; 3] = ["a\n", "b\n", "c\n"];

    struct Two {
        runs: AtomicU32,
        state: AtomicU8,
        plant: CoveragePlant,
    }

    impl Two {
        fn tree(&self) -> blaze_proc_macro::script_sandbox::Tree {
            let mut files = super::super::engine_script_reads_tests::status_files(false);
            let bytes = F[usize::from(self.state.load(Ordering::SeqCst))].as_bytes();
            files.iter_mut().find(|(name, _)| name == "f").expect("the fixture tracks f").1 = bytes.to_vec();
            blaze_proc_macro::script_sandbox::mem_tree(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())), "", None)
        }
    }

    impl crate::engine_scripts::ScriptRunner for Two {
        fn checkout_tree(&self) -> Option<blaze_proc_macro::script_sandbox::Tree> {
            Some(self.tree())
        }

        fn coverage_plant(&self) -> CoveragePlant {
            self.plant
        }

        fn run(&self, cx: &Cx, _: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<crate::engine_scripts::KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let frame = cx.frame();
            let (plant, view) = kept.expect("the request plans the reads' cells, so a member's reads are kept");
            let tree = self.tree();
            // Both scripts observe the same two answers (the double does not tell them apart).
            let recorded: std::vec::Vec<(&str, Option<blaze_ids::ContentId>)> = ["git-scope:pkg", ASKED]
                .into_iter()
                .map(|subject| (subject, blaze_proc_macro::script_sandbox::answer_now(&tree, subject).expect("a supported question")))
                .collect();
            let mut checkout = reads::cells_of(core::iter::empty(), plant).expect("no read has a cell");
            reads::collect_reads(recorded.into_iter(), plant, view, &mut checkout).expect("a status read has a cell");
            let mut cells: blaze_own::Vec<(blaze_drive::Name, blaze_ids::ContentId), &bstd::mem::Frame> = blaze_own::Vec::with_capacity_in(checkout.len(), frame);
            for read in checkout.iter() {
                cells.push((read.name(), read.answer));
            }
            let erased = blaze_own::Erased::new(frame);
            let outputs = blaze_build_script::outputs::Outputs::read(b"cargo::rustc-cfg=pkg_flag\n", "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle()).expect("the double's output reads");
            let did = crate::scripts::outputs_record(name, &cells[..], crate::scripts::RanAs::Wasm, &outputs, "a member 0.1.0");
            crate::scripts::PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept: None }
        }
    }

    fn world(seed: u64) -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        io.with_sim(|sim| sim.set_journal(blaze_core::sim::Journal::FsEvents)).expect("a simulated world");
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"pkg\", \"app\"]\nresolver = \"2\"\n");
        write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
        write(&io, "pkg/build.rs", "fn main() {\n    println!(\"cargo::rustc-cfg=pkg_flag\");\n}\n");
        write(&io, "pkg/src/lib.rs", "#[cfg(pkg_flag)]\npub fn f() -> u8 {\n    1\n}\n");
        write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[dependencies]\npkg = { path = \"../pkg\" }\n");
        write(&io, "app/build.rs", "fn main() {\n    println!(\"cargo::rustc-cfg=pkg_flag\");\n}\n");
        write(&io, "app/src/lib.rs", "pub fn g() -> u8 {\n    pkg::f()\n}\n");
        write(&io, "f", F[0]);
        io
    }

    /// `f` to `state` in the world (the journal names it) and in the served tree, and `app`'s body.
    fn edit(io: &Io, runner: &Two, state: u8, body: &str) {
        write(io, "f", F[usize::from(state)]);
        runner.state.store(state, Ordering::SeqCst);
        write(io, "app/src/lib.rs", &format!("pub fn g() -> u8 {{\n    {body}\n}}\n"));
    }

    fn status_line(io: &Io) -> std::string::String {
        super::super::engine_check_tests::profile(io).lines().find(|line| line.starts_with("sweep: status:")).map(std::string::String::from).unwrap_or_default()
    }

    /// One command: the scripts' runs after it, its status counts, and what it printed.
    fn command(io: &Io, runner: &'static Two) -> Result<(u32, crate::engine_script_reads::StatusCounts, std::string::String), std::string::String> {
        let before = stdout(io).len();
        let (ids, checked) = crate::engine_scripts::with_runner(runner, || engine_from(io, 1, false));
        let said = stdout(io).split_off(before);
        if !ids.is_empty() {
            return Err(format!("the answer refuses something: {ids:?}\n{said}"));
        }
        let status = checked.sweep.status.ok_or_else(|| format!("a served checkout and no status counts\n{said}"))?;
        Ok((runner.runs.load(Ordering::SeqCst), status, said))
    }

    /// The three commands, each held to its floor; `Err` names the first that missed it.
    /// `pad`: that many unrelated files written before the first command, untracked (the
    /// question asks `--untracked-files=no`), so the journal's place the first command keeps,
    /// and with it the coverage record's key, differs from one `pad` to the next.
    fn three_commands(seed: u64, plant: CoveragePlant, pad: u32) -> Result<(), std::string::String> {
        let runner: &'static Two = std::boxed::Box::leak(std::boxed::Box::new(Two { runs: AtomicU32::new(0), state: AtomicU8::new(0), plant }));
        let io = world(seed);
        for at in 0..pad {
            write(&io, &format!("pad/{at}"), "unrelated\n");
        }
        let (runs, status, said) = command(&io, runner)?;
        println!("1 clean: {}", status_line(&io));
        if runs != 2 || status.questions != 1 || status.kept != Ok(1) {
            return Err(format!("1: both scripts run once and the one status question is kept: runs {runs}, {status:?}\n{said}"));
        }
        edit(&io, runner, 1, "pkg::f() + 1");
        let (runs, status, said) = command(&io, runner)?;
        let line = status_line(&io);
        println!("2 edited: {line}");
        if status.incremental != 1 || status.whole != 0 || status.coverage != Ok(1) {
            return Err(format!("2: the status is answered from the kept answer and the journal's names: {status:?}\n{line}\n{said}"));
        }
        if !line.starts_with("sweep: status: 1 question(s): 1 from the previous answer") {
            return Err(format!("2: the profile says how the status was made: {line}"));
        }
        if runs != 4 {
            return Err(format!("2: the status moved (` M f`): both scripts run again: runs {runs}\n{said}"));
        }
        edit(&io, runner, 2, "pkg::f() + 2");
        let (runs, status, said) = command(&io, runner)?;
        let line = status_line(&io);
        println!("3 edited again: {line}");
        if status.incremental != 1 || status.whole != 0 {
            return Err(format!("3: the status is answered from the kept answer: {status:?}\n{line}\n{said}"));
        }
        if runs != 4 {
            return Err(format!("3: the status text is unchanged: no script runs: runs {runs}\n{said}"));
        }
        if (status.lists, status.ignore_reads) != (0, 0) {
            return Err(format!("3: an edit of a tracked file lists no folder and reads no ignore file: {status:?}\n{line}"));
        }
        Ok(())
    }

    #[test]
    fn a_status_after_an_edit_is_answered_from_the_last_and_the_journal() {
        three_commands(61, CoveragePlant::None, 0).unwrap_or_else(|why| panic!("{why}"));
    }

    /// ONE FORM: the next command finds the last one's status answer whatever key the coverage
    /// record has. Its key moves with the journal's place, and a root's keys are stored sorted
    /// (blaze-cas `sorted_keys`): read as "the root's first key" beside its answers, as until
    /// 2026-11-21, the coverage was found only where its key sorted first (tempo: "previous
    /// answers none: a record of another form" on about half the commands). Twelve places, so
    /// both orders of the coverage's key against the answer's occur (each about one time in
    /// two); every one must answer command 2 from the previous answer, 0 folders listed at 3.
    #[test]
    fn the_previous_status_answer_is_found_at_every_place() {
        for pad in 0..12 {
            three_commands(63, CoveragePlant::None, pad).unwrap_or_else(|why| panic!("place moved by {pad} file(s): {why}"));
        }
    }

    #[test]
    fn control_a_dropped_coverage_answers_the_status_whole() {
        let why = three_commands(62, CoveragePlant::Dropped, 0).expect_err("the oracle catches the dropped coverage");
        assert!(why.contains("NoPrevious") && why.contains("NoRoot"), "the next command has no previous answer, and says why: {why}");
    }
}
