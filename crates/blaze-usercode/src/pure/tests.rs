use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use blaze_cas::Config;
use blaze_core::{FileName, Untraceable, run_until_idle, sim::Faults, sim::Sim};

use super::*;
use crate::{
    run::OutFile,
    trace::{DIRECTORY, Entry, Names},
};

fn proof() -> Proof {
    Proof {
        identity: ContentId::of("blaze v2 test 1", b"pure script"),
        env: vec!["OUT_DIR".into(), "TARGET".into()],
        reads: true,
        writes: true,
        prints: true,
    }
}

const TOOLS: &[&str] = &["/toolchain"];

fn places(toolchain: &[PathBuf]) -> Places<'_> {
    Places {
        package: Path::new("/ws/p"),
        out_dir: Path::new("/ws/p/target/debug/build/p-0123/out"),
        target_dir: Path::new("/ws/p/target"),
        program: Path::new("/ws/p/target/debug/build/p-4567/build-script-build"),
        toolchain,
    }
}

fn traced(files: &[&str]) -> Traced {
    Traced {
        files: files.iter().map(|it| (PathBuf::from(it), Some([7; 32]))).collect(),
        dirs: Vec::new(),
        env: vec![("PATH".into(), Some("/bin".into()))],
        untraceable: Vec::new(),
    }
}

#[test]
fn a_pure_key_is_its_identity_and_never_a_sandboxed_runs() {
    let one = run_key(&proof());
    assert_eq!(one, run_key(&proof()));
    let mut other = proof();
    other.identity = ContentId::of("blaze v2 test 1", b"another script");
    assert_ne!(one, run_key(&other));
    assert_eq!(one.code, proof().identity);
    assert_ne!(one.given, Given::new(RunKind::BuildScript).finish(), "a pure key is given that it is one");
}

#[test]
fn the_trace_gives_the_package_files_and_passes_what_the_runtime_read() {
    let tools: Vec<PathBuf> = TOOLS.iter().map(PathBuf::from).collect();
    let program = places(&tools).program.to_str().unwrap().to_owned();
    let runtime = format!("{}/libc.so.6", RUNTIME[0]);
    let trace = traced(&[
        "/ws/p/build.rs.in",
        "/ws/p/target/debug/build/p-0123/out/generated.rs",
        &program,
        &runtime,
        "/toolchain/lib/libstd.so",
    ]);
    let read = check("p v0.1.0", &proof(), &places(&tools), &trace).unwrap();
    assert_eq!(read, vec![(PathBuf::from("/ws/p/build.rs.in"), Some([7; 32]))]);
}

#[test]
fn an_effect_the_proof_said_absent_is_an_error_naming_the_script() {
    let tools: Vec<PathBuf> = TOOLS.iter().map(PathBuf::from).collect();
    let places = places(&tools);
    let refused = |trace: Traced, proof: &Proof| {
        let error = check("p v0.1.0", proof, &places, &trace).unwrap_err();
        assert!(error.0.contains("`p v0.1.0`") && error.0.contains("a bug in blaze's purity proof"), "{}", error.0);
        error.0
    };
    assert!(refused(traced(&["/etc/passwd"]), &proof()).contains("/etc/passwd"));
    assert!(refused(traced(&["/ws/other/Cargo.toml"]), &proof()).contains("/ws/other/Cargo.toml"));
    assert!(refused(traced(&["/ws/p/target/debug/deps/x.rlib"]), &proof()).contains("target directory"));
    let mut reads_nothing = proof();
    reads_nothing.reads = false;
    assert!(refused(traced(&["/ws/p/data.txt"]), &reads_nothing).contains("in its package"));
    let mut clock = traced(&[]);
    clock.untraceable.push(Untraceable::Clock);
    assert!(refused(clock, &proof()).contains("Clock"));
    let mut listed = traced(&[]);
    listed.dirs.push(("/ws/p/src".into(), [1; 32]));
    assert!(refused(listed, &proof()).contains("listed /ws/p/src"));
}

#[test]
fn what_a_run_printed_and_wrote_is_checked_against_the_proof() {
    let value = |stdout: &[u8], files: Vec<OutFile>| Value { code: Some(0), signal: None, stdout: stdout.to_vec(), stderr: Vec::new(), files };
    let file = || OutFile { path: "x.rs".into(), executable: false, bytes: Some(b"fn x() {}".to_vec()) };
    checked_value("p", &proof(), &value(b"cargo:rustc-cfg=x\n", vec![file()])).unwrap();
    let mut silent = proof();
    silent.prints = false;
    assert!(checked_value("p", &silent, &value(b"cargo:rustc-cfg=x\n", Vec::new())).is_err());
    checked_value("p", &silent, &value(b"", Vec::new())).unwrap();
    let mut writes_nothing = proof();
    writes_nothing.writes = false;
    assert!(checked_value("p", &writes_nothing, &value(b"", vec![file()])).is_err());
}

struct Fixed {
    files: BTreeMap<PathBuf, [u8; 32]>,
    env: Vec<(OsString, OsString)>,
}

impl Inputs for Fixed {
    type Error = String;
    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(match path == Path::new("/ws/p") {
            true => Some(DIRECTORY),
            false => self.files.get(path).copied(),
        })
    }
    fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }
    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
    }
    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.sort();
        &self.env
    }
    fn names(&mut self, _: &Path, _: &mut Names<'_>) -> Result<bool, String> {
        Ok(false)
    }
    fn entry(&mut self, path: &Path) -> Result<Entry, String> {
        Ok(match self.files.get(path) {
            Some(hash) => Entry::File(ContentId(*hash), blaze_vfs::Mode::Regular),
            None => Entry::Missing,
        })
    }
}

fn opened(sim: &mut Sim) -> Store {
    let mut effects = Vec::new();
    let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
    let mut store = Store::open(config, &mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    });
    assert!(store.ready());
    store
}

#[test]
fn a_kept_pure_result_is_found_until_a_variable_or_file_it_read_changes() {
    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let env = || -> Vec<(OsString, OsString)> {
        vec![
            ("OUT_DIR".into(), "/ws/p/target/debug/build/p-0123/out".into()),
            ("TARGET".into(), "x86_64-unknown-linux-gnu".into()),
            ("PATH".into(), "/bin".into()),
        ]
    };
    let mut inputs = Fixed { files: [(PathBuf::from("/ws/p/data.txt"), [3; 32])].into(), env: env() };
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: b"cargo:rustc-cfg=x\n".to_vec(),
        stderr: Vec::new(),
        files: vec![OutFile { path: "x.rs".into(), executable: false, bytes: Some(b"fn x() {}".to_vec()) }],
    };
    let read = vec![(PathBuf::from("/ws/p/data.txt"), Some([3; 32]))];
    keep(&mut store, &proof(), read, &mut inputs, &value).unwrap();
    let reader = store.reader();
    let guard = blaze_cas::enter();
    assert_eq!(find(&reader, &guard, &proof(), &mut inputs).unwrap(), Some(value.clone()));

    // A variable passed through that the code never reads changes nothing.
    inputs.env.retain(|it| it.0 != "PATH");
    inputs.env.push(("PATH".into(), "/usr/bin".into()));
    assert_eq!(find(&reader, &guard, &proof(), &mut inputs).unwrap(), Some(value.clone()));

    inputs.env = env();
    inputs.env.retain(|it| it.0 != "TARGET");
    inputs.env.push(("TARGET".into(), "aarch64-unknown-linux-gnu".into()));
    assert_eq!(find(&reader, &guard, &proof(), &mut inputs).unwrap(), None, "a variable the code reads changed");

    inputs.env = env();
    inputs.files.insert("/ws/p/data.txt".into(), [4; 32]);
    assert_eq!(find(&reader, &guard, &proof(), &mut inputs).unwrap(), None, "a file it read changed");

    inputs.files.insert("/ws/p/data.txt".into(), [3; 32]);
    let mut other = proof();
    other.identity = ContentId::of("blaze v2 test 1", b"edited script");
    assert_eq!(find(&reader, &guard, &other, &mut inputs).unwrap(), None, "its code changed");
}

#[test]
fn a_miss_allocates_nothing_past_the_key() {
    let mut sim = Sim::new(6, Faults::default());
    let store = opened(&mut sim);
    let reader = store.reader();
    let guard = blaze_cas::enter();
    let mut inputs = Fixed { files: BTreeMap::new(), env: Vec::new() };
    let proof = proof();
    let found = blaze_alloc_count::assert_at_most(0, "a pure find that misses", || find(&reader, &guard, &proof, &mut inputs).unwrap());
    assert_eq!(found, None);
}
