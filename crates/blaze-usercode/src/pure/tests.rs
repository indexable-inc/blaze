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

fn roots() -> crate::roots::RootMap {
    crate::roots::RootMap::default()
        .with(crate::roots::Role::Package, Path::new("/ws/p"))
        .with(crate::roots::Role::OutDir, Path::new("/ws/p/target/debug/build/p-0123/out"))
        .with(crate::roots::Role::Target, Path::new("/ws/p/target"))
}

fn places(toolchain: &[PathBuf]) -> Places<'_> {
    Places {
        package: Path::new("/ws/p"),
        out_dir: Path::new("/ws/p/target/debug/build/p-0123/out"),
        target_dir: Path::new("/ws/p/target"),
        program: Path::new("/ws/p/target/debug/build/p-4567/build-script-build"),
        toolchain,
        search: &[],
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
    let one = run_key(&proof(), &roots().view());
    assert_eq!(one, run_key(&proof(), &roots().view()));
    let mut other = proof();
    other.identity = ContentId::of("blaze v2 test 1", b"another script");
    assert_ne!(one, run_key(&other, &roots().view()));
    assert_eq!(one.code, proof().identity);
    assert_ne!(
        one.given,
        Given::new(RunKind::BuildScript).finish(),
        "a pure key is given that it is one"
    );
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
        assert!(
            error.0.contains("`p v0.1.0`") && error.0.contains("a bug in blaze's purity proof"),
            "{}",
            error.0
        );
        error.0
    };
    assert!(refused(traced(&["/etc/passwd"]), &proof()).contains("/etc/passwd"));
    assert!(refused(traced(&["/ws/other/Cargo.toml"]), &proof()).contains("/ws/other/Cargo.toml"));
    assert!(
        refused(traced(&["/ws/p/target/debug/deps/x.rlib"]), &proof()).contains("target directory")
    );
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

/// A pure script refused for randomness says which process read it, by which system call and
/// from where: on 2026-11-10 the error said `Random` alone, and the script's processes could
/// not be traced from outside (its launcher starts them).
#[test]
fn an_untraceable_read_names_the_process_that_made_it() {
    use crate::calls::{self, Call, Made, Module, Phase};
    let tools: Vec<PathBuf> = TOOLS.iter().map(PathBuf::from).collect();
    let places = places(&tools);
    let made = Made {
        what: "random".to_owned(),
        pid: 4711,
        inside_pid: Some(7),
        program: places.program.display().to_string(),
        argv0: "build-script-build".to_owned(),
        syscall: "getrandom".to_owned(),
        number: 318,
        phase: Phase::Loading,
        ip: 0x7f1c_2a0e_4b2d,
        module: "/usr/lib/libc.so.6".to_owned(),
        kind: Module::Libc,
    };
    calls::left(places.program, vec![Call::Made(made)]);
    let mut random = traced(&[]);
    random.untraceable.push(Untraceable::Random);
    let error = check("p v0.1.0", &proof(), &places, &random).unwrap_err().0;
    calls::left(places.program, Vec::new());
    assert!(error.starts_with("the build script of `p v0.1.0` was proved pure"), "{error}");
    for part in [
        "(Random)",
        "by process 4711 (7 in the sandbox)",
        "(argv[0] `build-script-build`), made `getrandom` (system call 318)",
        "from libc (`/usr/lib/libc.so.6`, at 0x7f1c2a0e4b2d)",
        "before the program's `main` (a heuristic",
    ] {
        assert!(error.contains(part), "{part}: {error}");
    }
    // With no caller described, the error says so: never a caller of another kind or run.
    let error = check("p v0.1.0", &proof(), &places, &random).unwrap_err().0;
    assert!(error.contains("which process read it is not known"), "{error}");
}

/// A trace of `(path, hash)` files, as a run's sandbox assembles it.
fn traced_as(files: &[(&str, Option<[u8; 32]>)]) -> Traced {
    Traced {
        files: files.iter().map(|(path, hash)| (PathBuf::from(path), *hash)).collect(),
        dirs: Vec::new(),
        env: Vec::new(),
        untraceable: Vec::new(),
    }
}

/// A shared library's name on this machine, as its loader looks for one.
fn library(stem: &str) -> String {
    match Loader::HOST {
        Loader::Elf => format!("lib{stem}.so.1"),
        Loader::Dyld => format!("lib{stem}.dylib"),
        Loader::Windows => format!("{stem}.dll"),
    }
}

#[test]
fn a_library_name_is_the_loaders_and_nothing_else() {
    let named = |name: &str, loader| library_name(OsStr::new(name), loader);
    for name in ["libc.so.6", "libdl.so.2", "libgcc_s.so.1", "libz.so.1.3.1", "libfoo.so"] {
        assert!(named(name, Loader::Elf), "{name}");
    }
    for name in [
        "libfoo.rlib",
        "libfoo.rmeta",
        "libfoo.a",
        "libfoo.so.d",
        "libfoo.so.",
        "libfoo.so.1.",
        "libfoo.so..1",
        "libfoo.so.1a",
        "libfoo.so.so",
        "libfoo.sox",
        "lib.so",
        "foo.so",
        "foo.d",
        "libfoo",
        "libfoo.dylib",
        "foo.dll",
        "",
    ] {
        assert!(!named(name, Loader::Elf), "{name}");
    }
    assert!(named("libSystem.B.dylib", Loader::Dyld) && named("foo.dylib", Loader::Dyld));
    for name in [".dylib", "libfoo.so", "libfoo.dylib.d", "libfoo.rlib", "foo.dll"] {
        assert!(!named(name, Loader::Dyld), "{name}");
    }
    assert!(named("foo.dll", Loader::Windows) && named("KERNEL32.DLL", Loader::Windows));
    for name in [".dll", "foo.dll.d", "foo.lib", "foo.exe", "libfoo.so"] {
        assert!(!named(name, Loader::Windows), "{name}");
    }
}

/// The invariant: a read is the loader's walk of the run's library search path exactly when
/// it is a search directory (there or not) or a library's name directly in one; a name found
/// there is a load, which no pure key covers.
#[test]
fn a_probe_is_a_search_directory_or_a_library_name_directly_in_one() {
    let search = [PathBuf::from("/t/debug"), PathBuf::from("/t/debug/deps")];
    let at = |path: &str, hash: Option<[u8; 32]>| probe(Path::new(path), hash.as_ref(), &search);
    let lib = library("gcc_s");
    assert_eq!(at("/t/debug", Some(DIRECTORY)), Some(Probe::Directory));
    assert_eq!(at("/t/debug/deps", None), Some(Probe::Directory));
    assert_eq!(at(&format!("/t/debug/{lib}"), None), Some(Probe::Missed));
    assert_eq!(at(&format!("/t/debug/deps/{lib}"), None), Some(Probe::Missed));
    assert_eq!(at(&format!("/t/debug/deps/{lib}"), Some([7; 32])), Some(Probe::Found));
    // No search directory: the target directory, a directory beside or below one, a file
    // where one is named.
    assert_eq!(at("/t", Some(DIRECTORY)), None);
    assert_eq!(at("/t/debug/build", Some(DIRECTORY)), None);
    assert_eq!(at("/t/release", None), None);
    assert_eq!(at("/t/debug", Some([7; 32])), None);
    // A library's name where the loader does not look, and another file where it does.
    assert_eq!(at(&format!("/t/debug/build/{lib}"), None), None);
    assert_eq!(at(&format!("/t/release/{lib}"), None), None);
    assert_eq!(at(&format!("/t/debug/deps/sub/{lib}"), None), None);
    assert_eq!(at("/t/debug/deps/libserde-0123.rlib", None), None);
    assert_eq!(at("/t/debug/deps/serde-0123.d", Some([7; 32])), None);
    assert_eq!(at("/t/debug/.fingerprint", None), None);
    // With no search path nothing is a probe.
    assert_eq!(probe(Path::new("/t/debug"), None, &[]), None);
    assert_eq!(probe(Path::new(&format!("/t/debug/{lib}")), None, &[]), None);
}

/// glibc's `glibc-hwcaps/<level>` under a search directory (x86-64, POWER, s390x): the level's
/// directory and a library's name in it, and nothing deeper or under another name.
#[cfg(target_os = "linux")]
#[test]
fn glibc_looks_under_a_search_directorys_hwcaps_levels() {
    let search = [PathBuf::from("/t/debug")];
    let at = |path: &str, hash: Option<[u8; 32]>| probe(Path::new(path), hash.as_ref(), &search);
    assert_eq!(at("/t/debug/glibc-hwcaps/x86-64-v3", None), Some(Probe::Directory));
    assert_eq!(at("/t/debug/glibc-hwcaps/x86-64-v3/libc.so.6", None), Some(Probe::Missed));
    assert_eq!(at("/t/debug/glibc-hwcaps/x86-64-v2/libc.so.6", Some([7; 32])), Some(Probe::Found));
    assert_eq!(at("/t/debug/glibc-hwcaps", None), None);
    assert_eq!(at("/t/debug/glibc-hwcaps/x86-64-v3/x.rlib", None), None);
    assert_eq!(at("/t/debug/glibc-hwcaps/a/b/libc.so.6", None), None);
    assert_eq!(at("/t/debug/other/x86-64-v3/libc.so.6", None), None);
    assert_eq!(at("/t/debug/build/glibc-hwcaps/x86-64-v3/libc.so.6", None), None);
}

/// The whole read set of a pure script's run on NixOS aarch64 with glibc 2.44 (2026-11-11:
/// zstd-safe, rustls, parking_lot_core, getrandom, crossbeam-utils, serde_json, anyhow, libc),
/// as `blaze explain script` gave it: each was refused one build at a time. All of it is the
/// loader's, and the check passes it with no package file read.
#[cfg(target_os = "linux")]
#[test]
fn the_loaders_reads_before_main_are_passed_whole() {
    const T: &str = "/root/m1/.cache/blaze/build/b-main-0123/";
    const GLIBC: &str = "/nix/store/abc-glibc-2.44/";
    let t = |rest: &str| format!("{T}{rest}");
    let glibc = |rest: &str| format!("{GLIBC}{rest}");
    let out_dir = t("debug/build/p-0123/out");
    let program = t("debug/build/p-4567/build-script-build");
    let search = [PathBuf::from(t("debug")), PathBuf::from(t("debug/deps"))];
    let tools: Vec<PathBuf> = TOOLS.iter().map(PathBuf::from).collect();
    let places = Places {
        package: Path::new("/root/m1/registry/p-0.1.0"),
        out_dir: Path::new(&out_dir),
        target_dir: Path::new(T),
        program: Path::new(&program),
        toolchain: &tools,
        search: &search,
    };
    let (found, dir, missed) = (Some([7; 32]), Some(DIRECTORY), None);
    let reads = [
        (String::from("/etc/ld-nix.so.preload"), missed),
        (glibc("etc/ld.so.cache"), found),
        (glibc("lib"), dir),
        (glibc("lib/libc.so.6"), found),
        (glibc("lib/libdl.so.2"), found),
        (glibc("lib/libgcc_s.so.1"), found),
        (t("debug"), dir),
        (t("debug/deps"), dir),
        (t("debug/deps/libgcc_s.so.1"), missed),
        (t("debug/libc.so.6"), missed),
        (t("debug/libdl.so.2"), missed),
        (t("debug/libgcc_s.so.1"), missed),
        (program.clone(), found),
    ];
    let with = |more: &[(&str, Option<[u8; 32]>)]| {
        let mut all: Vec<(&str, Option<[u8; 32]>)> =
            reads.iter().map(|(path, hash)| (path.as_str(), *hash)).collect();
        all.extend_from_slice(more);
        check("p v0.1.0", &proof(), &places, &traced_as(&all)).map_err(|it| it.0)
    };
    assert_eq!(with(&[]), Ok(Vec::new()));
    // The same walk on x86-64, where glibc has hardware levels.
    let level = t("debug/glibc-hwcaps/x86-64-v3");
    let leveled = t("debug/glibc-hwcaps/x86-64-v3/libc.so.6");
    assert_eq!(with(&[(&level, missed), (&leveled, missed)]), Ok(Vec::new()));

    // Planted: each a read of the script's own, still refused, by its path.
    let refused = |path: &str, hash, why: &str| {
        let error = with(&[(path, hash)]).unwrap_err();
        assert!(error.contains(path) && error.contains(why), "{path}: {error}");
    };
    // Another unit's output and a file that is no library, in a search directory.
    refused(&t("debug/deps/libserde-0123.rlib"), found, "in the target directory");
    refused(&t("debug/deps/libserde-0123.rlib"), missed, "in the target directory");
    refused(&t("debug/.fingerprint"), missed, "in the target directory");
    refused(&t("debug/notes.txt"), found, "in the target directory");
    // A library's name in directories not on the search path.
    refused(&t("debug/build/libgcc_s.so.1"), missed, "in the target directory");
    refused(&t("release/libc.so.6"), missed, "in the target directory");
    refused("/opt/lib/libc.so.6", missed, "outside its package and OUT_DIR");
    // The target directory itself, and a directory that is none of the path's.
    refused(T.trim_end_matches('/'), dir, "in the target directory");
    refused(&t("debug/build"), dir, "in the target directory");
    // A file under another package.
    refused("/root/m1/registry/q-0.2.0/Cargo.toml", found, "outside its package and OUT_DIR");
    // A library found on the search path was loaded: an input no pure key covers.
    refused(&t("debug/deps/libgcc_s.so.1"), found, "its pure key does not cover");
    refused(&t("debug/libplugin.so"), found, "its pure key does not cover");
    // A search directory listed: the loader lists none.
    let mut listed = traced_as(&[]);
    listed.dirs.push((PathBuf::from(t("debug/deps")), [1; 32]));
    let error = check("p v0.1.0", &proof(), &places, &listed).unwrap_err().0;
    assert!(error.contains("listed") && error.contains("debug/deps"), "{error}");
}

/// A search directory outside the target directory (one the build was started with in its
/// library path variable) is walked the same; one inside the package of a script that may
/// read is kept as the package's file it is, in the key.
#[test]
fn a_search_directory_outside_the_target_directory_is_walked_the_same() {
    let tools: Vec<PathBuf> = TOOLS.iter().map(PathBuf::from).collect();
    let search = [PathBuf::from("/opt/lib"), PathBuf::from("/ws/p/vendor")];
    let places = Places { search: &search, ..places(&tools) };
    let lib = library("z");
    let (outside, inside) = (format!("/opt/lib/{lib}"), format!("/ws/p/vendor/{lib}"));
    let trace = traced_as(&[("/opt/lib", Some(DIRECTORY)), (&outside, None), (&inside, None)]);
    let read = check("p v0.1.0", &proof(), &places, &trace).unwrap();
    assert_eq!(read, vec![(PathBuf::from(&inside), None)]);
    let mut reads_nothing = proof();
    reads_nothing.reads = false;
    assert_eq!(check("p v0.1.0", &reads_nothing, &places, &trace).unwrap(), Vec::new());
    let loaded = traced_as(&[(&outside, Some([7; 32]))]);
    let error = check("p v0.1.0", &proof(), &places, &loaded).unwrap_err().0;
    assert!(error.contains(&outside) && error.contains("its pure key does not cover"), "{error}");
    let other = traced_as(&[("/opt/lib/pkgconfig/z.pc", None)]);
    let error = check("p v0.1.0", &proof(), &places, &other).unwrap_err().0;
    assert!(error.contains("outside its package and OUT_DIR"), "{error}");
}

#[test]
fn what_a_run_printed_and_wrote_is_checked_against_the_proof() {
    let value = |stdout: &[u8], files: Vec<OutFile>| Value {
        code: Some(0),
        signal: None,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
        files,
    };
    let file =
        || OutFile { path: "x.rs".into(), executable: false, bytes: Some(b"fn x() {}".to_vec()) };
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
    let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48, load_tags: (1 << 48) + (1 << 32), loader: blaze_cas::Loader::Pumped };
    let mut store = Store::open(config, &mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    }).expect("the effects end");
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
    let mut inputs =
        Fixed { files: [(PathBuf::from("/ws/p/data.txt"), [3; 32])].into(), env: env() };
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: b"cargo:rustc-cfg=x\n".to_vec(),
        stderr: Vec::new(),
        files: vec![OutFile {
            path: "x.rs".into(),
            executable: false,
            bytes: Some(b"fn x() {}".to_vec()),
        }],
    };
    let read = vec![(PathBuf::from("/ws/p/data.txt"), Some([3; 32]))];
    let map = roots();
    keep(&mut store, &proof(), roots().view(), read, &mut inputs, &value).unwrap();
    let reader = store.reader();
    let guard = blaze_cas::enter();
    assert_eq!(
        find(&reader, &guard, &proof(), map.view(), &mut inputs).unwrap(),
        Some(value.clone())
    );

    // A variable passed through that the code never reads changes nothing.
    inputs.env.retain(|it| it.0 != "PATH");
    inputs.env.push(("PATH".into(), "/usr/bin".into()));
    assert_eq!(
        find(&reader, &guard, &proof(), map.view(), &mut inputs).unwrap(),
        Some(value.clone())
    );

    inputs.env = env();
    inputs.env.retain(|it| it.0 != "TARGET");
    inputs.env.push(("TARGET".into(), "aarch64-unknown-linux-gnu".into()));
    assert_eq!(
        find(&reader, &guard, &proof(), map.view(), &mut inputs).unwrap(),
        None,
        "a variable the code reads changed"
    );

    inputs.env = env();
    inputs.files.insert("/ws/p/data.txt".into(), [4; 32]);
    assert_eq!(
        find(&reader, &guard, &proof(), map.view(), &mut inputs).unwrap(),
        None,
        "a file it read changed"
    );

    inputs.files.insert("/ws/p/data.txt".into(), [3; 32]);
    let mut other = proof();
    other.identity = ContentId::of("blaze v2 test 1", b"edited script");
    assert_eq!(
        find(&reader, &guard, &other, map.view(), &mut inputs).unwrap(),
        None,
        "its code changed"
    );
}

#[test]
fn a_miss_allocates_nothing_past_the_key() {
    let mut sim = Sim::new(6, Faults::default());
    let store = opened(&mut sim);
    let reader = store.reader();
    let guard = blaze_cas::enter();
    let mut inputs = Fixed { files: BTreeMap::new(), env: Vec::new() };
    let proof = proof();
    let map = roots();
    let found = blaze_alloc_count::assert_at_most(0, "a pure find that misses", || {
        find(&reader, &guard, &proof, map.view(), &mut inputs).unwrap()
    });
    assert_eq!(found, None);
}

#[cfg(not(windows))] // Windows has no roots (`blaze_usercode::roots`).
#[test]
fn a_pure_result_kept_in_one_checkout_is_found_in_another() {
    let mut sim = Sim::new(7, Faults::default());
    let mut store = opened(&mut sim);
    let at = |home: &str| {
        let roots = crate::roots::RootMap::default()
            .with(crate::roots::Role::Package, Path::new(&format!("{home}/p")))
            .with(
                crate::roots::Role::OutDir,
                Path::new(&format!("{home}/p/target/debug/build/p-0123/out")),
            )
            .with(crate::roots::Role::Target, Path::new(&format!("{home}/p/target")));
        let inputs = Fixed {
            files: [(PathBuf::from(format!("{home}/p/data.txt")), [3; 32])].into(),
            env: vec![
                ("OUT_DIR".into(), format!("{home}/p/target/debug/build/p-0123/out").into()),
                ("TARGET".into(), "x86_64-unknown-linux-gnu".into()),
            ],
        };
        (roots, inputs)
    };
    let value = |home: &str| Value {
        code: Some(0),
        signal: None,
        stdout: format!("cargo:rustc-link-search={home}/p/target/debug/build/p-0123/out\n")
            .into_bytes(),
        stderr: Vec::new(),
        files: Vec::new(),
    };
    let (a, mut a_inputs) = at("/home/a");
    let read = |home: &str| vec![(PathBuf::from(format!("{home}/p/data.txt")), Some([3; 32]))];
    keep(&mut store, &proof(), a.view(), read("/home/a"), &mut a_inputs, &value("/home/a"))
        .unwrap();
    let (b, mut b_inputs) = at("/srv/b");
    {
        let reader = store.reader();
        let guard = blaze_cas::enter();
        // Found where it ran; never in another checkout on that one's word alone.
        assert_eq!(
            find(&reader, &guard, &proof(), a.view(), &mut a_inputs).unwrap(),
            Some(value("/home/a"))
        );
        assert_eq!(
            find(&reader, &guard, &proof(), b.view(), &mut b_inputs).unwrap(),
            None,
            "unverified"
        );
    }
    // B runs it and gets the same, its output ported: verified.
    keep(&mut store, &proof(), b.view(), read("/srv/b"), &mut b_inputs, &value("/srv/b")).unwrap();
    let reader = store.reader();
    let guard = blaze_cas::enter();
    let (c, mut c_inputs) = at("/opt/c");
    // Found, and what it printed is this checkout's.
    assert_eq!(
        find(&reader, &guard, &proof(), c.view(), &mut c_inputs).unwrap(),
        Some(value("/opt/c"))
    );
    c_inputs.files.insert("/opt/c/p/data.txt".into(), [4; 32]);
    assert_eq!(
        find(&reader, &guard, &proof(), c.view(), &mut c_inputs).unwrap(),
        None,
        "a file it read changed"
    );
}
