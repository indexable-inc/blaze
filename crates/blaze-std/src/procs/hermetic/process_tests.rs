//! The real tests of each `Level::Inputs` backend (kind `process`: they start processes; the
//! coordinator runs them). Each confines a tiny program, this test binary itself re-run as a
//! "hostile child" (the pattern of blaze-proc-macro's native_sandbox tests), under the plan the
//! shipped code builds, and asserts the OS lets it read a file inside its roots and refuses one
//! beside them.
//!
//! macOS: the shipped profile ([`super::seatbelt::profile`]) applied by `sandbox-exec`, as the
//! backend does. Linux: the shipped confinement ([`super::landlock::prepare`]) entered in the
//! child by blaze-sys `linux::confine::enter`, through std's `pre_exec` (test code only: it
//! proves the prepared plan confines; blaze-core's own confine step, which the shipped start
//! uses, has its kernel test in blaze-core `process/linked/run/confine_tests.rs`).

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    string::String,
    vec::Vec as StdVec,
};

use blaze_core::wait::Deadline;
use blaze_own::{AVec, Alloc};
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
use core::time::Duration;

use super::{
    Budget, Capture, ClockSpec, Fault, HermeticOpts, HermeticSpec, HermeticStreams, Level, Plan, PlanOpts, Program, ReportSpec,
    RootFiles,
};

const ATTEMPT: &str = "BLAZE_D177_READ";

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The roots' files: only a tree root and the output, so nothing is listed.
struct NoPackages;

impl RootFiles for NoPackages {
    fn package_files(&self, _: RootId, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Ok(())
    }
    fn seen(&self, _: RootId, _: RelPath<'_>) -> Result<Seen, Fault> {
        Ok(Seen::Absent)
    }
    fn read_output(&self, _: RelPath<'_>, _: u32, _: &mut AVec<'_, u8>) -> Result<(), Fault> {
        Ok(())
    }
    fn list_output(&self, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Ok(())
    }
    fn clear_output(&self) -> Result<(), Fault> {
        Ok(())
    }
}

/// The child: when `ATTEMPT` is set, runs a special mode (`@burn`, `@spawn_burn`, `@noop`,
/// `@rapid_children`) or reads the path it names and prints what the OS answered. Otherwise
/// nothing (it is also run as an ordinary test).
#[test]
fn hostile_child() {
    let Ok(path) = std::env::var(ATTEMPT) else { return };
    match path.as_str() {
        "@burn" => {
            let mut acc: u64 = 0;
            loop {
                acc = core::hint::black_box(acc.wrapping_add(1));
            }
        }
        "@spawn_burn" => {
            let exe = this_binary();
            let mut child = Command::new(exe).args(CHILD_ARGS).env_clear().env(ATTEMPT, "@burn").spawn().unwrap();
            let _ = child.wait();
            return;
        }
        "@noop" => return,
        "@rapid_children" => {
            let exe = this_binary();
            for _ in 0..24 {
                let status = Command::new(&exe).args(CHILD_ARGS).env_clear().env(ATTEMPT, "@noop").status().unwrap();
                assert!(status.success(), "noop child exited {status:?}");
            }
            return;
        }
        _ => {}
    }
    let said = match std::fs::read(&path) {
        Ok(_) => String::from("D177 READ OK"),
        Err(error) => std::format!("D177 READ REFUSED {:?}", error.kind()),
    };
    let mut out = std::io::stdout();
    writeln!(out, "{said}").unwrap();
    out.flush().unwrap();
}

/// A folder of this test's own: `inside/inside.txt` (a tree root), `out/` (the output root) and
/// `outside.txt` beside them, every path canonical (Seatbelt matches resolved paths, and
/// macOS's temporary folder is a link).
struct Folder {
    base: PathBuf,
}

impl Folder {
    fn new(name: &str) -> Folder {
        let base = std::env::temp_dir().join(std::format!("blaze-d177-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("inside")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        std::fs::write(base.join("inside/inside.txt"), b"inside").unwrap();
        std::fs::write(base.join("outside.txt"), b"outside").unwrap();
        Folder { base: std::fs::canonicalize(&base).unwrap() }
    }

    fn path(&self, rest: &str) -> String {
        self.base.join(rest).to_str().unwrap().into()
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.base);
    }
}

fn this_binary() -> String {
    std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap().to_str().unwrap().into()
}

const CHILD_ARGS: [&str; 4] = ["--exact", "procs::hermetic::process_tests::hostile_child", "--nocapture", "--test-threads=1"];

/// The plan of a spec over `folder`, as the backend makes it, with `system` the backend's set.
fn with_plan<R>(folder: &Folder, exe: &str, system: &[&str], body: impl FnOnce(&HermeticSpec<'_>, &Plan<'_>) -> R) -> R {
    let inside = folder.path("inside");
    let out = folder.path("out");
    let list = [Root { name: "inside", kind: RootKind::Tree { tree: Digest([4; 32]) } }, Root { name: "out", kind: RootKind::Output }];
    let roots = Roots::new(&list).unwrap();
    let mounts = [Mount { spelling: inside.as_bytes(), root: RootId(0) }, Mount { spelling: out.as_bytes(), root: RootId(1) }];
    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: "hostile child", digest: Digest([5; 32]), path: exe },
        args: &CHILD_ARGS,
        env: &[],
        roots,
        mounts: &mounts,
        cwd: &out,
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock: ClockSpec::NeverAsks,
        seed: 0,
        budget: Budget { cpu: Duration::from_secs(10), memory: 1 << 32, steps: None },
        deadline: Deadline::at(Duration::from_secs(60)),
        streams: HermeticStreams { stdout: Capture::Keep { most: 1 << 16 }, stderr: Capture::Keep { most: 1 << 16 } },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .unwrap();
    let plan = Plan::new(PlanOpts { spec: &spec, system, files: &NoPackages, alloc: heap() }, blaze_own::planted::NoPlant).unwrap();
    body(&spec, &plan)
}

/// The child's `D177 ...` words in `line`, from the marker to the line's end: libtest prints
/// `test <name> ... ` and the child's own line lands after it, on the same line.
fn marked(line: &str) -> Option<&str> {
    line.find("D177 ").map(|at| &line[at..])
}

/// The child's one `D177` line, or a panic naming how it ended, both its streams (on macOS
/// `sandbox-exec`'s own refusals are on standard error) and the confinement it ran under.
fn said(output: &std::process::Output, confinement: &str) -> String {
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: StdVec<&str> = text.lines().filter_map(marked).collect();
    assert_eq!(
        lines.len(),
        1,
        "the child ended {:?} and said {text:?} (stderr {:?}); it ran under:\n{confinement}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    lines[0].into()
}

/// Native world helper shared by macOS process tests (no duplicated `std` / `Vec` sites).
#[cfg(target_os = "macos")]
fn native_io(out: &str) -> crate::io::Io {
    crate::io::Io::native(crate::io::NativeWorld {
        root: Path::new(out),
        threads: 1,
        env: crate::env::Vars::given(StdVec::new()),
        args: crate::args::ArgList::given(StdVec::new()),
        host_name: std::sync::Arc::from(""),
        seed: Some(0),
        above: 0,
        limits: crate::io::Limits::USUAL,
    })
    .expect("a native world over the test's folder")
}

/// `program` run through the SHIPPED path, [`super::Hermetic::run`] (the plan, the profile,
/// `sandbox-exec` started by blaze-core's linked start, the port's pipes, meter and wait), with
/// `reads` each a tree root and the folder's `out/` the output root and working folder: its
/// standard output when it exits 0, else a panic naming how it ended (or the refusal), both
/// streams and the confinement it ran under ([`super::confinement`]).
#[cfg(target_os = "macos")]
fn run_shipped(folder: &Folder, program: &str, args: &[&str], env: &[(&str, &str)], helpers: &[&str], reads: &[&str]) -> String {
    let out = folder.path("out");
    let mut list = StdVec::new();
    for _ in reads {
        list.push(Root { name: "a folder the program reads", kind: RootKind::Tree { tree: Digest([4; 32]) } });
    }
    list.push(Root { name: "out", kind: RootKind::Output });
    let roots = Roots::new(&list).unwrap();
    let mut mounts: StdVec<Mount<'_>> = reads.iter().enumerate().map(|(at, path)| Mount { spelling: path.as_bytes(), root: RootId(at as u8) }).collect();
    mounts.push(Mount { spelling: out.as_bytes(), root: RootId(reads.len() as u8) });
    let io = native_io(&out);
    let spawner = io.need_host().and_then(|host| host.spawner()).expect("a native world's spawner");
    let clock = io.need_clock().expect("a native world's clock");
    let cx = io.cx(Duration::from_secs(120), crate::mem::TASK_FRAME).expect("a test's root frame");
    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: program, digest: Digest([5; 32]), path: program },
        args,
        env,
        roots,
        mounts: &mounts,
        cwd: &out,
        helpers,
        helper_folders: &[],
        channels: &[],
        clock: ClockSpec::NeverAsks,
        seed: 0,
        budget: Budget { cpu: Duration::from_secs(10), memory: 1 << 32, steps: None },
        deadline: Deadline::after(clock.now().since_start(), Duration::from_secs(60)),
        streams: HermeticStreams { stdout: Capture::Keep { most: 1 << 16 }, stderr: Capture::Keep { most: 1 << 16 } },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .unwrap_or_else(|error| panic!("{program}: the spec was refused: {error}"));
    let confinement = super::confinement(&spec, &NoPackages, heap()).unwrap();
    let confinement = String::from_utf8_lossy(&confinement).into_owned();
    let hermetic = super::Hermetic::new(super::HermeticCapOpts { spawner, clock });
    let ran = match hermetic.run(&cx, &spec, &NoPackages, heap()) {
        Ok(ran) => ran,
        Err(error) => panic!("{program}: the run was refused: {error}; it was to run under:\n{confinement}"),
    };
    let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
    assert_eq!(ran.exit, crate::procs::Exit::Exited(0), "{program}: ended {:?}; stdout {stdout:?}; stderr {stderr:?}; it ran under:\n{confinement}", ran.exit);
    stdout
}

#[cfg(target_os = "macos")]
blaze_sim::kind_test! {
    /// The ladder that catches a profile no program can start under, cheapest first, through
    /// the shipped path: `/usr/bin/true`, then `/bin/sh -c 'echo ok'` (macOS's `/bin/sh` starts
    /// the shell `/private/var/select/sh` names: bash, dash or zsh, so those are its helpers and
    /// that folder a root), then this test binary reading a file inside its roots. Each must
    /// exit 0 under its profile; a failure prints how it ended, both streams and the profile.
    /// Prints nothing; passes.
    process fn seatbelt_runs_true_then_sh_then_this_binary() {
        let folder = Folder::new("ladder");
        assert_eq!(run_shipped(&folder, "/usr/bin/true", &[], &[], &[], &[]), "");
        let shells = ["/bin/bash", "/bin/dash", "/bin/zsh"];
        assert_eq!(run_shipped(&folder, "/bin/sh", &["-c", "echo ok"], &[], &shells, &["/private/var/select"]), "ok\n");
        let exe = this_binary();
        let inside = folder.path("inside/inside.txt");
        let inside_folder = folder.path("inside");
        let said = run_shipped(&folder, &exe, &CHILD_ARGS, &[(ATTEMPT, inside.as_str())], &[], &[inside_folder.as_str()]);
        let words: StdVec<&str> = said.lines().filter_map(marked).collect();
        assert_eq!(words, ["D177 READ OK"], "this binary said {said:?}");
    }
}

#[cfg(target_os = "macos")]
blaze_sim::kind_test! {
    /// Process-group metering and `ESRCH` exit-race safety on macOS:
    /// 1. An idle sandboxed parent spawning a CPU-burning child must be stopped by `Over::Cpu`
    ///    (proving `proc_listpgrppids` + group rusage meters helpers, not only the leader).
    /// 2. A sandboxed parent rapidly spawning and reaping short-lived children across many meter
    ///    ticks must complete cleanly with exit 0 (proving `ESRCH` on exiting PIDs never aborts).
    process fn seatbelt_meters_child_cpu_and_handles_rapid_exit() {
        let folder = Folder::new("meter-group");
        let out = folder.path("out");
        let exe = this_binary();
        let list = [Root { name: "out", kind: RootKind::Output }];
        let roots = Roots::new(&list).unwrap();
        let mounts = [Mount { spelling: out.as_bytes(), root: RootId(0) }];
        let io = native_io(&out);
        let spawner = io.need_host().and_then(|host| host.spawner()).expect("a native world's spawner");
        let clock = io.need_clock().expect("a native world's clock");
        let cx = io.cx(Duration::from_secs(60), crate::mem::TASK_FRAME).expect("a test's root frame");
        let hermetic = super::Hermetic::new(super::HermeticCapOpts {
            spawner,
            clock: io.need_clock().expect("a native world's clock"),
        });

        // 1. Idle parent spawns a CPU-burning child: group metering stops it with Over::Cpu well
        //    before the 10-second wall deadline.
        let burn_env = [(ATTEMPT, "@spawn_burn")];
        let burn_spec = HermeticSpec::new(HermeticOpts {
            program: Program::Stored { name: &exe, digest: Digest([6; 32]), path: &exe },
            args: &CHILD_ARGS,
            env: &burn_env,
            roots,
            mounts: &mounts,
            cwd: &out,
            helpers: &[],
            helper_folders: &[],
            channels: &[],
            clock: ClockSpec::NeverAsks,
            seed: 0,
            budget: Budget { cpu: Duration::from_millis(80), memory: 1 << 32, steps: None },
            deadline: Deadline::after(clock.now().since_start(), Duration::from_secs(10)),
            streams: HermeticStreams { stdout: Capture::Keep { most: 1 << 16 }, stderr: Capture::Keep { most: 1 << 16 } },
            level: Level::Inputs,
            report: ReportSpec::WholeRoots,
        })
        .unwrap();
        let err = hermetic.run(&cx, &burn_spec, &NoPackages, heap()).err().expect("CPU-burning child must hit budget");
        assert!(
            matches!(err.fault, Fault::Stopped(super::Stop::Budget(super::Over::Cpu { .. }))),
            "expected Over::Cpu from child CPU metering, got {err}"
        );

        // 2. Rapidly spawning/exiting short-lived children across many meter intervals must not
        //    trip ESRCH errors.
        let rapid_env = [(ATTEMPT, "@rapid_children")];
        let rapid_spec = HermeticSpec::new(HermeticOpts {
            program: Program::Stored { name: &exe, digest: Digest([7; 32]), path: &exe },
            args: &CHILD_ARGS,
            env: &rapid_env,
            roots,
            mounts: &mounts,
            cwd: &out,
            helpers: &[],
            helper_folders: &[],
            channels: &[],
            clock: ClockSpec::NeverAsks,
            seed: 0,
            budget: Budget { cpu: Duration::from_secs(10), memory: 1 << 32, steps: None },
            deadline: Deadline::after(clock.now().since_start(), Duration::from_secs(30)),
            streams: HermeticStreams { stdout: Capture::Keep { most: 1 << 16 }, stderr: Capture::Keep { most: 1 << 16 } },
            level: Level::Inputs,
            report: ReportSpec::WholeRoots,
        })
        .unwrap();
        let ran = hermetic
            .run(&cx, &rapid_spec, &NoPackages, heap())
            .unwrap_or_else(|error| panic!("rapid child exit must not fail metering: {error}"));
        assert_eq!(ran.exit, crate::procs::Exit::Exited(0));
    }
}

#[cfg(target_os = "macos")]
blaze_sim::kind_test! {
    process fn seatbelt_refuses_a_read_outside_the_roots() {
        let folder = Folder::new("seatbelt");
        let exe = this_binary();
        with_plan(&folder, &exe, &super::seatbelt::SYSTEM, |spec, plan| {
            let profile = super::seatbelt::profile(plan, heap()).unwrap();
            let text = std::str::from_utf8(&profile[..profile.len() - 1]).unwrap();
            let attempt = |path: &str| {
                let output = Command::new("/usr/bin/sandbox-exec")
                    .arg("-p")
                    .arg(text)
                    .arg(&exe)
                    .args(spec.args())
                    .env_clear()
                    .env(ATTEMPT, path)
                    .current_dir(Path::new(spec.cwd()))
                    .output()
                    .unwrap();
                said(&output, text)
            };
            assert_eq!(attempt(&folder.path("inside/inside.txt")), "D177 READ OK");
            assert!(attempt(&folder.path("outside.txt")).starts_with("D177 READ REFUSED"));
        });
    }
}

#[cfg(target_os = "linux")]
blaze_sim::kind_test! {
    process fn landlock_refuses_a_read_outside_the_roots() {
        use std::os::unix::process::CommandExt as _;
        let folder = Folder::new("landlock");
        let exe = this_binary();
        with_plan(&folder, &exe, &super::landlock::SYSTEM, |spec, plan| {
            let prepared = super::landlock::prepare(plan, spec.budget(), false).unwrap();
            let rendered = String::from_utf8_lossy(&super::confinement(spec, &NoPackages, heap()).unwrap()).into_owned();
            let attempt = |path: &str| {
                let mut command = Command::new(&exe);
                command.args(spec.args()).env_clear().env(ATTEMPT, path).current_dir(Path::new(spec.cwd()));
                // `pre_exec` takes a 'static closure: the parent's confinement is copied into it
                // (the ruleset by number: the parent keeps it open until the child has run).
                let confine = prepared.confine();
                let ruleset = confine.ruleset.raw();
                let filter = confine.filter.to_vec();
                let limits = confine.limits.to_vec();
                // SAFETY: between fork and exec the closure makes only system calls (prctl,
                // prlimit64, landlock_restrict_self, seccomp) on memory the parent made before
                // the fork; it allocates nothing and takes no lock.
                unsafe {
                    command.pre_exec(move || {
                        let confine = blaze_sys::linux::confine::ChildConfine { ruleset: blaze_sys::Fd::new(ruleset), filter: &filter, limits: &limits };
                        blaze_sys::linux::confine::enter(&confine)
                            .map_err(|(_, errno)| std::io::Error::from_raw_os_error(i32::from(errno.raw())))
                    });
                }
                said(&command.output().unwrap(), &rendered)
            };
            assert_eq!(attempt(&folder.path("inside/inside.txt")), "D177 READ OK");
            assert!(attempt(&folder.path("outside.txt")).starts_with("D177 READ REFUSED"));
        });
    }
}
