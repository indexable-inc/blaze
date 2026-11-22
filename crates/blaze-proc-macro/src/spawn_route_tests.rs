//! The spawn router (spawn_route.rs) and spawn answers as records (spawn_records.rs), driven
//! through `ScriptSandbox::spawn`, the one host call a wasm build script's spawn makes (the
//! request bytes are the shim's encoding, guest/script_process.rs), with no hypervisor and no
//! process: the executors here are models written in this file.
//!
//! What is right, from the plan (READINESS.md 1a, 1i) and std's `Command` (library/std/src/
//! process.rs, `Command::env`, `env_remove`, `env_clear`: "the child inherits the parent's
//! environment unless `env_clear` is called; later calls for one key replace earlier ones").

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use blaze_build_script::facts::Fact;
use blaze_build_script::key::{CMode, RunLimits};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_spawn::{Executor, Files, MACHINE_V1, NoSandbox, Personality, SpawnAnswer, SpawnRefused, SpawnRequest, Status, Tool, Written};

use crate::record::{DirStore, native_world};
use crate::sandbox::Read;
use crate::script::{ScriptError, ScriptWhy};
use crate::script_sandbox::{Refusal, Script, ScriptSandbox, SpawnRefusal, Tree};
use crate::spawn_records::Recorded;
use crate::spawn_route::{Decoded, PinnedTool, Route, SpawnSetting, child_env, hex, program_found, route};

const LIMITS: RunLimits = RunLimits { fuel: 1 << 26, memory: 16 << 20, out_dir_bytes: 1 << 20, out_dir_files: 16, output: 1 << 20, line: 256 << 10 };

/// A spawn request in the shim's encoding.
pub(crate) fn shim(argv: &[&str], env_clear: bool, env_ops: &[(bool, &str, &str)], cwd: Option<&str>, stdin: &[u8], modes: [u8; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    let put = |out: &mut Vec<u8>, bytes: &[u8]| {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    };
    out.extend_from_slice(&(argv.len() as u32).to_le_bytes());
    for arg in argv {
        put(&mut out, arg.as_bytes());
    }
    out.push(u8::from(env_clear));
    out.extend_from_slice(&(env_ops.len() as u32).to_le_bytes());
    for (set, name, value) in env_ops {
        out.push(u8::from(*set));
        put(&mut out, name.as_bytes());
        put(&mut out, value.as_bytes());
    }
    out.push(u8::from(cwd.is_some()));
    put(&mut out, cwd.unwrap_or("").as_bytes());
    put(&mut out, stdin);
    out.extend_from_slice(&modes);
    out
}

pub(crate) fn script(c_mode: CMode, spawner: Option<Arc<dyn Executor + Send + Sync>>, spawn_setting: SpawnSetting) -> Arc<Script> {
    Arc::new(Script {
        package: "spawner 0.0.0".to_owned(),
        tree: Tree::Mem { files: Arc::new(vec![("gen.pl".to_owned(), b"print \"ret\\n\";\n".to_vec())]), git: None },
        // The workspace's root package: its tree is served at `/workspace` (the model reads
        // `/workspace/gen.pl`).
        root: crate::script_sandbox::ScriptRoot::member("").expect("the root package"),
        env: vec![("OUT_DIR".to_owned(), "/out".to_owned())],
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode,
        limits: LIMITS,
        table: Arc::new(Vec::new()),
        probes: None,
        spawner,
        step: ContentId([3; 32]),
        spawn_setting: Arc::new(spawn_setting),
        deps: Arc::new(crate::script_sandbox::dep_mount::DepViews::none()),
    })
}

fn kind(routed: &Route<'_>) -> &'static str {
    match routed {
        Route::NotFound => "not found",
        Route::Rustc => "rustc",
        Route::CargoVersion => "cargo version",
        Route::Git => "git",
        Route::Fact(Fact::PkgConfig { .. }) => "fact pkg-config",
        Route::Fact(Fact::SwVers { .. }) => "fact sw_vers",
        Route::Fact(Fact::CompilerVersion { .. }) => "fact compiler",
        Route::Fact(Fact::CcProbe { .. }) => "fact probe",
        Route::Deferred => "deferred",
        Route::Executor => "executor",
        Route::Refused => "refused",
    }
}

/// The order is fixed and decided from text: `rustc`, git, a fact, a deferred tool in check,
/// then the executor when there is one, else a refusal. `pkg-config` and `clang --version` are
/// C tools too (probe.rs `C_TOOLS`): the fact comes first, so they are never deferred.
#[test]
fn route_order_is_fixed() {
    let cases: &[(&str, &[&str], CMode, bool, &str)] = &[
        ("rustc", &["-vV"], CMode::Built, true, "rustc"),
        // codspeed's `cargo --version` (tempo's check, check-g051344): the pin's fact, with no
        // sandbox; any other cargo command is a program like the rest.
        ("cargo", &["--version"], CMode::Built, false, "cargo version"),
        ("/toolchain/bin/cargo", &["-V"], CMode::Deferred, false, "cargo version"),
        ("cargo", &["version"], CMode::Built, true, "cargo version"),
        ("cargo", &["-vV"], CMode::Built, false, "refused"),
        ("cargo", &["metadata"], CMode::Built, false, "refused"),
        ("git", &["rev-parse", "HEAD"], CMode::Built, true, "git"),
        ("sh", &["-c", "git rev-parse HEAD"], CMode::Built, true, "git"),
        ("pkg-config", &["--libs", "--cflags", "libusb-1.0"], CMode::Deferred, false, "fact pkg-config"),
        ("pkg-config", &["--libs", "--cflags", "libusb-1.0"], CMode::Built, true, "fact pkg-config"),
        ("clang", &["--version"], CMode::Deferred, false, "fact compiler"),
        ("clang", &["--version"], CMode::Built, true, "fact compiler"),
        // cc's probes are the pinned table's in every mode (c_tasks_probe), before deferring.
        ("cc", &["-E", "/out/1detect_compiler_family.c"], CMode::Deferred, false, "fact probe"),
        ("cc", &["-E", "/out/1detect_compiler_family.c"], CMode::Built, true, "fact probe"),
        ("c++", &["-?"], CMode::Built, false, "fact probe"),
        ("cc", &["-c", "x.c"], CMode::Deferred, false, "deferred"),
        ("cc", &["-c", "x.c"], CMode::Deferred, true, "deferred"),
        ("cc", &["-c", "x.c"], CMode::Built, true, "executor"),
        ("cc", &["-c", "x.c"], CMode::Built, false, "refused"),
        ("perl", &["x.pl"], CMode::Deferred, false, "deferred"),
        ("perl", &["x.pl"], CMode::Built, true, "executor"),
        ("perl", &["x.pl"], CMode::Built, false, "refused"),
        // A program of the pinned base tool set (D192) blaze does not answer: the executor's.
        ("protoc", &["--version"], CMode::Deferred, true, "executor"),
        ("python3", &["gen.py"], CMode::Built, false, "refused"),
        // A program no machine with the set has: the OS's `NotFound`, with or without an
        // executor, in every mode (libc 0.2.162..0.2.189's `emcc -dumpversion` on every target).
        ("emcc", &["-dumpversion"], CMode::Deferred, false, "not found"),
        ("emcc", &["-dumpversion"], CMode::Built, true, "not found"),
        ("bindgen", &["wrapper.h"], CMode::Deferred, true, "not found"),
        ("freebsd-version", &[], CMode::Deferred, false, "not found"),
    ];
    let alloc = blaze_own::testing::heap().alloc();
    for (name, args, c_mode, has_executor, want) in cases {
        let found = program_found(name, HOST, alloc).unwrap_or_else(|e| panic!("{e}"));
        let got = kind(&route(name, args, *c_mode, found, *has_executor));
        assert_eq!(got, *want, "`{name} {}` in {c_mode:?}, executor {has_executor}", args.join(" "));
    }
}

/// The host the tests' scripts run on (`script` below), a platform of pins/base-tools.json.
const HOST: &str = "aarch64-apple-darwin";

/// Every name an arm of `route` answers is found: the shim asks `found` before it spawns, so a
/// name the list missed would be `NotFound` to the script though blaze answers it. PLANTED: a
/// list without `sh` (whose `-c "git ..."` the git arm answers) is caught.
#[test]
fn found_covers_every_answered_route() {
    let alloc = blaze_own::testing::heap().alloc();
    let answered: &[(&str, &[&str])] = &[
        ("rustc", &["-vV"]),
        ("cargo", &["-V"]),
        ("git", &["rev-parse", "HEAD"]),
        ("sh", &["-c", "git rev-parse HEAD"]),
        ("bash", &["-c", "git rev-parse HEAD"]),
        ("sh", &["./configure"]),
        ("pkg-config", &["--libs", "x"]),
        ("pkgconf", &["--libs", "x"]),
        ("sw_vers", &["-productVersion"]),
        ("gcc", &["--version"]),
        ("c++", &["-?"]),
        ("xcrun", &["--show-sdk-path"]),
        ("gmake", &["all"]),
        ("perl", &["x.pl"]),
    ];
    let claimed_without_list = |name: &str, args: &[&str]| !matches!(kind(&route(name, args, CMode::Deferred, true, false)), "refused" | "executor");
    for (name, args) in answered {
        assert!(claimed_without_list(name, args), "`{name}` is answered by an arm of route");
        assert_eq!(program_found(name, HOST, alloc), Ok(true), "`{name} {}` is answered, so it is found", args.join(" "));
    }
    let planted = |name: &str| name != "sh" && program_found(name, HOST, alloc) == Ok(true);
    let missed: Vec<&str> = answered.iter().map(|(name, _)| *name).filter(|name| !planted(name)).collect();
    assert_eq!(missed, ["sh", "sh"], "the planted list without `sh` is caught");
    // A path is opened, not looked up on PATH: found, and routed by its last component.
    assert_eq!(program_found("/opt/emsdk/emcc", HOST, alloc), Ok(true));
    // A host with no base tool set cannot say: refused by name, never guessed (rule 3).
    let no_set = program_found("emcc", "x86_64-apple-darwin", alloc).expect_err("no set for x86_64-apple-darwin");
    assert!(no_set.to_string().contains("x86_64-apple-darwin") && no_set.to_string().contains("D192"), "{no_set}");
}

/// libc 0.2.186's `Command::new("emcc").arg("-dumpversion").output().ok()?` (turso, c1814):
/// the shim's `found` is answered "no" and the read `absent-program\n<host>\nemcc` recorded with
/// its answer; `fact_now` answers that read again equal while emcc is still missing. TWIN:
/// `python3`, in the set (and answered by no arm), is found and records nothing; a recorded
/// `absent-program` read of a program the list holds (the set grew) answers `None`, unequal to
/// any recorded answer, so the script runs again (rule 21). CONTROL: the same spawn from a
/// module whose shim did not ask (an old module) is refused by name, not answered.
#[test]
fn a_missing_program_is_not_found_and_its_absence_is_read() {
    use crate::fact_pins::{FactNow, fact_now};
    let alloc = blaze_own::testing::heap().alloc();
    let mut sandbox = ScriptSandbox::new(script(CMode::Deferred, None, SpawnSetting::off()));
    assert_eq!(sandbox.found(b"emcc").map_err(|stop| stop.0), Ok(false), "emcc: the OS's NotFound");
    let subject = format!("absent-program\n{HOST}\nemcc");
    let read = sandbox.reads.iter().find(|read: &&Read| read.subject == subject).unwrap_or_else(|| panic!("no {subject:?} read: {:?}", sandbox.reads));
    let Some(digest) = read.digest else { panic!("the absence was recorded without its answer") };
    assert_eq!(fact_now(&subject, None, alloc), FactNow::Answer(Some(digest)), "answered again equal, with no pins");
    assert!(crate::spawn_route::is_fact_reply(&subject, alloc), "kept as a derived fact's reply (scripts.rs `admit_reads`)");

    assert_eq!(sandbox.found(b"python3").map_err(|stop| stop.0), Ok(true), "TWIN: python3 is in the set");
    assert!(!sandbox.reads.iter().any(|read| read.subject.ends_with("\npython3")), "a found program records no absence");
    let grown = format!("absent-program\n{HOST}\npython3");
    assert_eq!(fact_now(&grown, None, alloc), FactNow::Answer(None), "TWIN: a program now in the list moves the read");

    let stop = match sandbox.spawn(&shim(&["emcc", "-dumpversion"], false, &[], None, b"", [2, 2])) {
        Ok(()) => panic!("an unasked spawn of a missing program was answered"),
        Err(stop) => stop,
    };
    assert!(
        matches!(&sandbox.refused, Some(Refusal::Spawn(SpawnRefusal::NotFoundUnasked { program, .. })) if program == "emcc"),
        "CONTROL: {:?} ({})",
        sandbox.refused,
        stop.0
    );
    assert!(stop.0.contains("rebuild the module"), "{}", stop.0);
}

fn decoded(env_clear: bool, env_ops: &[(bool, &str, &str)]) -> Decoded {
    Decoded {
        argv: vec!["perl".to_owned()],
        env_clear,
        env_ops: env_ops.iter().map(|(set, name, value)| (*set, (*name).to_owned(), (*value).to_owned())).collect(),
        cwd: None,
        stdin: Vec::new(),
        modes: [2, 2],
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
}

/// std's `Command`: inherit unless cleared; a set inserts or replaces, an unset removes, in
/// order; the result sorted by name.
#[test]
fn child_env_follows_command_semantics() {
    let parent = pairs(&[("B", "2"), ("A", "1")]);
    assert_eq!(child_env(&parent, &decoded(false, &[])), pairs(&[("A", "1"), ("B", "2")]), "inherited, sorted");
    assert_eq!(
        child_env(&parent, &decoded(false, &[(true, "C", "3"), (true, "A", "9"), (false, "B", "")])),
        pairs(&[("A", "9"), ("C", "3")]),
        "set inserts, set replaces, unset removes"
    );
    assert_eq!(child_env(&parent, &decoded(true, &[(true, "Z", "1"), (false, "Z", ""), (true, "Y", "2")])), pairs(&[("Y", "2")]), "cleared first, then in order");
    assert_eq!(child_env(&parent, &decoded(false, &[(true, "X", "1"), (true, "X", "2")])), pairs(&[("A", "1"), ("B", "2"), ("X", "2")]), "the later set wins");
    assert_eq!(child_env(&parent, &decoded(false, &[(false, "Q", "")])), pairs(&[("A", "1"), ("B", "2")]), "unsetting an absent name is nothing");
}

/// The oracle: `perl` in a build with no executor is refused, never deferred (a deferred
/// tool's empty answer in a build would be a wrong output, not an error).
fn refuses_perl_in_a_build(route_of: impl for<'s> Fn(&'s str, &'s [&'s str], CMode, bool, bool) -> Route<'s>) -> Result<(), &'static str> {
    match kind(&route_of("perl", &["x.pl"], CMode::Built, true, false)) {
        "refused" => Ok(()),
        other => Err(other),
    }
}

/// READINESS.md 1i: a script spawning a tool with the sandbox off stops with one line naming
/// the program, the sandbox and the decision (D92), and the owner's (D94); the error the
/// runner gives names the package. PLANTED: a route that defers `perl` in a build is caught.
#[test]
fn a_script_spawning_perl_with_the_sandbox_off_is_refused_by_name() {
    assert_eq!(refuses_perl_in_a_build(route), Ok(()), "the router refuses perl in a build with the sandbox off");
    let planted = refuses_perl_in_a_build(|name, args, c_mode, found, has_executor| {
        if name == "perl" && c_mode == CMode::Built { Route::Deferred } else { route(name, args, c_mode, found, has_executor) }
    });
    assert_eq!(planted, Err("deferred"), "the planted router (perl deferred in a build) is caught");

    let script = script(CMode::Built, None, SpawnSetting::off());
    let mut sandbox = ScriptSandbox::new(Arc::clone(&script));
    let stop = match sandbox.spawn(&shim(&["perl", "x.pl"], false, &[], None, b"", [2, 2])) {
        Ok(()) => panic!("perl ran with the sandbox off"),
        Err(stop) => stop,
    };
    assert!(stop.0.contains("`perl x.pl`") && stop.0.contains("D92") && stop.0.contains("D94"), "{}", stop.0);
    let Some(refusal) = sandbox.refused.clone() else { panic!("no refusal kept: {}", stop.0) };
    assert!(
        matches!(&refusal, Refusal::Spawn(SpawnRefusal::NoSandbox { program, why: NoSandbox::OwnerHasNotAllowed, .. }) if program == "perl"),
        "{refusal:?}"
    );
    let error = ScriptError { package: script.package.clone(), why: ScriptWhy::Refused(refusal) };
    let text = error.to_string();
    assert!(text.contains("spawner 0.0.0") && text.contains("perl"), "{text}");
}

/// A model of the sandbox: writes `/out/x.S`, prints a `cargo:` line, keeps the key it saw.
struct Model {
    seen: Mutex<Option<ContentId>>,
}

impl Executor for Model {
    fn name(&self) -> &'static str {
        "model"
    }

    fn answer<'a>(&self, request: &SpawnRequest<'_>, files: &dyn Files, alloc: Alloc<'a>) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        *self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(request.key(alloc));
        let refused = |what: &str| SpawnRefused::Refused {
            program: blaze_own::String::from_str_in("perl", alloc),
            what: blaze_own::String::from_str_in(what, alloc),
        };
        // The script the tool runs is read through the request's grant.
        if files.bytes("/workspace/gen.pl").is_none() {
            return Err(refused("the model found no /workspace/gen.pl through its files"));
        }
        let mut bytes = blaze_own::Vec::new_in(alloc);
        bytes.extend_from_slice(b"ret\n");
        let mut written = blaze_own::Vec::new_in(alloc);
        written.push(Written { path: blaze_own::String::from_str_in("/out/x.S", alloc), bytes: Some(bytes) });
        let mut stdout = blaze_own::Vec::new_in(alloc);
        stdout.extend_from_slice(b"cargo:rustc-cfg=from_child\n");
        SpawnAnswer::new(Status::Code(0), stdout, blaze_own::Vec::new_in(alloc), written, blaze_own::Vec::new_in(alloc))
            .map_err(|why| refused(&format!("the model's answer: {why:?}")))
    }
}

/// The executor's written file lands in `OUT_DIR`, where the script then reads it; the child's
/// inherited stdout joins the script's (its `cargo:` lines count as the script's); the read
/// list holds `spawn:<request key hex>`.
#[test]
fn a_model_executor_answers_and_its_files_land_in_out_dir() {
    let model = Arc::new(Model { seen: Mutex::new(None) });
    let setting = SpawnSetting {
        hvbox_allowed: true,
        tools: vec![PinnedTool { name: "perl".to_owned(), digest: [7; 32], guest_path: "/usr/bin/perl".to_owned() }],
        userland: Some(ContentId([9; 32])),
        pins: None,
    };
    let spawner: Arc<dyn Executor + Send + Sync> = Arc::clone(&model) as Arc<dyn Executor + Send + Sync>;
    let mut sandbox = ScriptSandbox::new(script(CMode::Built, Some(spawner), setting));
    if let Err(stop) = sandbox.spawn(&shim(&["perl", "gen.pl"], false, &[], None, b"", [0, 2])) {
        panic!("the model's spawn stopped: {}", stop.0);
    }
    assert_eq!(&sandbox.reply[..4], &0i32.to_le_bytes(), "exit 0");
    match sandbox.snippet_file("/out", "x.S") {
        Ok(Some(bytes)) => assert_eq!(bytes, b"ret\n", "the script reads what the tool wrote"),
        Ok(None) => panic!("OUT_DIR holds no x.S after the spawn"),
        Err(stop) => panic!("reading x.S stopped: {}", stop.0),
    }
    assert!(sandbox.stdout.windows(26).any(|w| w == b"cargo:rustc-cfg=from_child"), "the child's inherited stdout is the script's");
    let Some(key) = *model.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) else { panic!("the model was not asked") };
    let subject = format!("spawn:{}", hex(&key.0));
    assert!(sandbox.reads.iter().any(|read: &Read| read.subject == subject && read.digest.is_some()), "the read list holds {subject}: {:?}", sandbox.reads);
}

/// An executor that counts the calls it answers.
struct Counting<'c> {
    calls: &'c AtomicU32,
}

impl Executor for Counting<'_> {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn answer<'a>(&self, _request: &SpawnRequest<'_>, _files: &dyn Files, alloc: Alloc<'a>) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut stdout = blaze_own::Vec::new_in(alloc);
        stdout.extend_from_slice(b"23.1.4\n");
        SpawnAnswer::new(Status::Code(0), stdout, blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc)).map_err(|why| {
            SpawnRefused::Refused {
                program: blaze_own::String::from_str_in("cc", alloc),
                what: blaze_own::String::from_str_in(&format!("the counting answer: {why:?}"), alloc),
            }
        })
    }
}

struct NoFiles;

impl Files for NoFiles {
    fn bytes(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

/// Rule 77: the second spawn of one request is the record's, the executor asked once.
#[test]
fn recorded_answers_a_second_time_without_the_inner_executor() {
    let alloc = Alloc::global();
    let store = std::env::temp_dir().join(format!("blaze-spawn-records-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).expect("a temporary folder");
    let (io, root) = native_world(&store).unwrap_or_else(|error| panic!("the records' world: {error}"));
    let records = DirStore::build_scripts(io.spawner(), io.boot().ok(), &root);
    let calls = AtomicU32::new(0);
    let recorded = Recorded { inner: Counting { calls: &calls }, records: &records };
    let text = |value: &str| blaze_own::String::from_str_in(value, alloc);
    let request = SpawnRequest::new(
        ContentId([1; 32]),
        Personality::Xnu,
        MACHINE_V1,
        ContentId([2; 32]),
        Tool { name: text("cc"), digest: [5; 32], guest_path: text("/usr/bin/cc") },
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
        text("/workspace"),
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
    )
    .unwrap_or_else(|why| panic!("the request: {why:?}"));
    let mut bytes = [Vec::new(), Vec::new()];
    for held in &mut bytes {
        let answer = recorded.answer(&request, &NoFiles, alloc).unwrap_or_else(|why| panic!("{why}"));
        let mut encoded = blaze_own::Vec::new_in(alloc);
        answer.encode(&mut encoded);
        held.extend_from_slice(encoded.as_slice());
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1, "the inner executor answered once for two spawns of one key");
    assert_eq!(bytes[0], bytes[1], "the record answers with the first answer's bytes");
    let _ = std::fs::remove_dir_all(&store);
}

// ---- A deferred request's inputs are captured at its spawn (outputs.rs, THE C INPUTS) ----
//
// The task side (blaze-build-script c_tasks_tests.rs, `a_source_the_script_*`) holds that
// each compile reads its state; these hold that the script's record holds each state.

const X_A: &[u8] = b"int x(void) { return 0xA; }\n";
const X_B: &[u8] = b"int x(void) { return 0xB; }\n";

/// The script writes (or, with `None`, deletes) `name` in `OUT_DIR`, as its `fd_write` and
/// `path_unlink_file` do: through [`crate::script_sandbox::OutDir::edit`].
fn put(sandbox: &mut ScriptSandbox, name: &str, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => {
            sandbox.out.edit().insert(name.to_owned(), Some(bytes.to_vec()));
        }
        None => {
            sandbox.out.edit().remove(name);
        }
    }
}

fn defer(sandbox: &mut ScriptSandbox, argv: &[&str]) {
    if let Err(stop) = sandbox.spawn(&shim(argv, false, &[], None, b"", [2, 2])) {
        panic!("`{}` stopped: {}", argv.join(" "), stop.0);
    }
    assert_eq!(&sandbox.reply[..4], &0i32.to_le_bytes(), "a deferred request answers exit 0");
}

/// The script's record as script.rs makes it at the run's end: its files, then its requests
/// and their states (`c_inputs`).
fn record_of<'a>(sandbox: &ScriptSandbox, alloc: Alloc<'a>) -> blaze_own::AVec<'a, u8> {
    let mut outputs = blaze_build_script::Outputs::new_in("/out", alloc);
    for (name, bytes) in sandbox.out_files() {
        outputs.add_file(name, bytes, blaze_build_script::outputs::FileLimits { bytes: 1 << 20, files: 16 }, alloc).expect("a file of OUT_DIR");
    }
    crate::script::c_inputs(sandbox, &mut outputs, alloc);
    let mut bytes = blaze_own::Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    bytes
}

/// (capture a) aws-lc-sys's `compiler_check`: the script writes `gone.c`, defers its compile,
/// then deletes it. The record holds the state the compile saw, `gone.c`'s bytes in it, and a
/// driver lays them (`laid_state`). CONTROL: `OUT_DIR` as the script left it has no `gone.c`.
#[test]
fn a_deferred_compile_keeps_a_source_the_script_deleted() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut sandbox = ScriptSandbox::new(script(CMode::Deferred, None, SpawnSetting::off()));
    put(&mut sandbox, "gone.c", Some(X_A));
    defer(&mut sandbox, &["cc", "-o", "/out/gone.o", "-c", "/out/gone.c"]);
    put(&mut sandbox, "gone.c", None);
    let record = record_of(&sandbox, alloc);
    let outputs = blaze_build_script::outputs::OutputsRef::open(&record).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(outputs.c_request_states().collect::<Vec<_>>(), [0]);
    let laid = blaze_build_script::c_tasks::laid_state(&outputs, 0, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(laid.as_slice(), [("gone.c", Some(X_A))], "the compile's state holds the source as written");
    assert!(outputs.file("gone.c").is_none(), "CONTROL: the record's OUT_DIR holds no gone.c: reading it at run time finds nothing");
}

/// (capture b) The script writes `x.c` = A, defers, rewrites `x.c` = B, defers again, then
/// defers an `ar` with no edit between: three requests, two states (the `ar` takes the second
/// again, by the edit count), A held by digest and B the record's own final file, kept once.
/// CONTROL: the record's `OUT_DIR` holds B only: a driver reading it at run time compiles B
/// for both.
#[test]
fn a_deferred_compile_keeps_each_version_of_a_rewritten_source() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut sandbox = ScriptSandbox::new(script(CMode::Deferred, None, SpawnSetting::off()));
    put(&mut sandbox, "x.c", Some(X_A));
    defer(&mut sandbox, &["cc", "-o", "/out/x1.o", "-c", "/out/x.c"]);
    put(&mut sandbox, "x.c", Some(X_B));
    defer(&mut sandbox, &["cc", "-o", "/out/x2.o", "-c", "/out/x.c"]);
    defer(&mut sandbox, &["ar", "cqD", "/out/libx.a", "/out/x1.o", "/out/x2.o"]);
    let record = record_of(&sandbox, alloc);
    let outputs = blaze_build_script::outputs::OutputsRef::open(&record).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(outputs.c_request_states().collect::<Vec<_>>(), [0, 1, 1]);
    assert_eq!(outputs.c_state_count(), 2);
    let snaps = |state| outputs.c_state(state).expect("the state").collect::<Vec<_>>();
    let a = blaze_build_script::outputs::snap_digest(X_A);
    assert_eq!(snaps(0), [("x.c", blaze_build_script::outputs::Snap::Held(a))]);
    assert_eq!(snaps(1), [("x.c", blaze_build_script::outputs::Snap::Final)], "B is the final file, not held twice");
    let lay = |state| blaze_build_script::c_tasks::laid_state(&outputs, state, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((lay(0).as_slice(), lay(1).as_slice()), ([("x.c", Some(X_A))].as_slice(), [("x.c", Some(X_B))].as_slice()));
    assert!(outputs.c_held(&blaze_build_script::outputs::snap_digest(X_B)).is_none(), "B's bytes are only the file's");
    let last = match outputs.file("x.c") {
        Some(blaze_build_script::outputs::OutFileRef::Inline(bytes)) => bytes,
        other => panic!("x.c: {other:?}"),
    };
    assert_eq!(last, X_B, "CONTROL: OUT_DIR read at run time is B for both compiles");
}
