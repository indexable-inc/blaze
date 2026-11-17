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
use crate::spawn_route::{Decoded, PinnedTool, Route, SpawnSetting, child_env, hex, route};

const LIMITS: RunLimits = RunLimits { fuel: 1 << 26, memory: 16 << 20, out_dir_bytes: 1 << 20, out_dir_files: 16, output: 1 << 20, line: 256 << 10 };

/// A spawn request in the shim's encoding.
fn shim(argv: &[&str], env_clear: bool, env_ops: &[(bool, &str, &str)], cwd: Option<&str>, stdin: &[u8], modes: [u8; 2]) -> Vec<u8> {
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

fn script(c_mode: CMode, spawner: Option<Arc<dyn Executor + Send + Sync>>, spawn_setting: SpawnSetting) -> Arc<Script> {
    Arc::new(Script {
        package: "spawner 0.0.0".to_owned(),
        tree: Tree::Mem { files: Arc::new(vec![("gen.pl".to_owned(), b"print \"ret\\n\";\n".to_vec())]), git: None },
        folder: String::new(),
        env: vec![("OUT_DIR".to_owned(), "/out".to_owned())],
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode,
        limits: LIMITS,
        table: Arc::new(Vec::new()),
        spawner,
        step: ContentId([3; 32]),
        spawn_setting: Arc::new(spawn_setting),
    })
}

fn kind(routed: &Route<'_>) -> &'static str {
    match routed {
        Route::Rustc => "rustc",
        Route::Git => "git",
        Route::Fact(Fact::PkgConfig { .. }) => "fact pkg-config",
        Route::Fact(Fact::SwVers { .. }) => "fact sw_vers",
        Route::Fact(Fact::CompilerVersion { .. }) => "fact compiler",
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
        ("git", &["rev-parse", "HEAD"], CMode::Built, true, "git"),
        ("sh", &["-c", "git rev-parse HEAD"], CMode::Built, true, "git"),
        ("pkg-config", &["--libs", "--cflags", "libusb-1.0"], CMode::Deferred, false, "fact pkg-config"),
        ("pkg-config", &["--libs", "--cflags", "libusb-1.0"], CMode::Built, true, "fact pkg-config"),
        ("clang", &["--version"], CMode::Deferred, false, "fact compiler"),
        ("clang", &["--version"], CMode::Built, true, "fact compiler"),
        ("cc", &["-c", "x.c"], CMode::Deferred, false, "deferred"),
        ("cc", &["-c", "x.c"], CMode::Deferred, true, "deferred"),
        ("cc", &["-c", "x.c"], CMode::Built, true, "executor"),
        ("cc", &["-c", "x.c"], CMode::Built, false, "refused"),
        ("perl", &["x.pl"], CMode::Deferred, false, "deferred"),
        ("perl", &["x.pl"], CMode::Built, true, "executor"),
        ("perl", &["x.pl"], CMode::Built, false, "refused"),
        ("bindgen", &["wrapper.h"], CMode::Deferred, true, "executor"),
        ("bindgen", &["wrapper.h"], CMode::Deferred, false, "refused"),
    ];
    for (name, args, c_mode, has_executor, want) in cases {
        let got = kind(&route(name, args, *c_mode, *has_executor));
        assert_eq!(got, *want, "`{name} {}` in {c_mode:?}, executor {has_executor}", args.join(" "));
    }
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
fn refuses_perl_in_a_build(route_of: impl for<'s> Fn(&'s str, &'s [&'s str], CMode, bool) -> Route<'s>) -> Result<(), &'static str> {
    match kind(&route_of("perl", &["x.pl"], CMode::Built, false)) {
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
    let planted = refuses_perl_in_a_build(|name, args, c_mode, has_executor| {
        if name == "perl" && c_mode == CMode::Built { Route::Deferred } else { route(name, args, c_mode, has_executor) }
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
    let records = DirStore::build_scripts(&io, &root);
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
