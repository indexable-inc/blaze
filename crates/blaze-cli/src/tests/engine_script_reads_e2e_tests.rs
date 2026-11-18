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
}

impl crate::engine_scripts::ScriptRunner for Double {
    fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<ReadsPlant>, _: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
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
        let Some(stage_plant) = kept else {
            let mut words = blaze_own::Vec::new_in(std::alloc::Global);
            words.extend_from_slice(b"the double read the checkout on a path that keeps no reads");
            return pooled(crate::scripts::Ran::Failed { what: "a member's build script read a checkout file the drive cannot name yet", words, hole: true }, std::vec::Vec::new());
        };
        let plant = if self.plant == ReadsPlant::None { stage_plant } else { self.plant };
        let cells = reads::cells_of(recorded.iter().map(|(subject, digest)| (subject.as_str(), *digest)), plant).expect("every read has a cell");
        let read_rows: std::vec::Vec<_> = cells.iter().map(|read| (read.name(), read.answer)).collect();
        let erased = blaze_own::Erased::new(std::alloc::Global);
        let printed: &[u8] = if flag { b"cargo::rustc-cfg=pkg_flag\n" } else { b"" };
        let outputs = blaze_build_script::outputs::Outputs::read(printed, "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle()).expect("the double's output reads");
        match crate::scripts::outputs_record(name, &read_rows[..], crate::scripts::RanAs::Wasm, &outputs, "pkg") {
            made @ crate::scripts::Ran::Made(_) => pooled(made, cells),
            failed => pooled(failed, std::vec::Vec::new()),
        }
    }
}

fn double(prints: Prints, plant: ReadsPlant) -> &'static Double {
    std::boxed::Box::leak(std::boxed::Box::new(Double { runs: AtomicU32::new(0), prints, plant }))
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

mod parallel {
    use super::*;
    use blaze_build_script::env::{DepMetadata, Given, PackageFacts, env};
    use blaze_core::lock::Lock;
    use blaze_drive::{Kind, Name};
    use crate::engine_scripts::{ScriptClock, ScriptRunner};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
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

        fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: Name, kept: Option<ReadsPlant>, producers: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
            let erased = blaze_own::Erased::new(cx.frame());
            let mut checkout = std::vec::Vec::new();
            if name == self.a || name == self.b {
                let (path, subject) = if name == self.a { ("a/input", "file:a/input") } else { ("b/input", "file:b/input") };
                let (_, _, answer) = reads::now(cx, root, &NoStamps, Plant::None, ReadKind::File, path, None, cx.frame()).unwrap();
                if name == self.a && answer == blaze_proc_macro::script_sandbox::file_answer(b"fail") {
                    let failed = crate::scripts::failed("a build script failed", format_args!("fixture producer a refused its input"), true);
                    return crate::scripts::PooledScript { did: Ok(failed), ns: 0, used: 0, checkout, unkept: None };
                }
                checkout = reads::cells_of([(subject, Some(answer))].into_iter(), kept.unwrap()).unwrap();
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
            let unkept = (name != self.a && name != self.b).then(|| reads::NoCell::Kind { subject: format!("fixture-{name}") });
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

    fn times(events: &[Event], script: Name) -> (u64, u64) {
        let mut starts = events.iter().filter_map(|event| match *event { Event::Start(n, t) if n == script => Some(t), _ => None });
        let mut ends = events.iter().filter_map(|event| match *event { Event::End(n, t) if n == script => Some(t), _ => None });
        let interval = (starts.next().expect("script started"), ends.next().expect("script ended"));
        assert_eq!((starts.next(), ends.next()), (None, None), "each script runs once");
        interval
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
}
