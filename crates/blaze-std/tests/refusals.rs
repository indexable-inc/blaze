//! Every compile-time refusal of bstd, compiled: bstd builds with `doctest = false`, so no
//! `compile_fail` example of its docs runs, and this is where each refusal is shown to refuse.
//!
//! `refusals/` holds one tiny crate per refusal, `<name>.rs`, whose first line is
//! `//! refused: <text>`, and its twin `<name>.twin.rs`, the same crate with the refused part
//! removed. Each pair is compiled with the compiler that built bstd (`BSTD_RUSTC`, handed over
//! by `build.rs` from cargo's `RUSTC`) against the bstd this test links: the refusal must fail
//! with `<text>` in what the compiler said, and its twin must compile, so a refusal that
//! passes for another reason (a typo, a missing feature) shows as its twin failing.
//!
//! Where bstd is: found from this test's binary ([`crate_dirs`]). In cargo's old layout the
//! binary is in `deps/`, beside the `libbstd-<hash>.rlib` it was linked with and every crate
//! bstd depends on; in the build directory's new layout each unit has its own
//! `build/<package>/<hash>/out/`, and every one of them is searched. Each fixture gets
//! `--extern bstd=<that rlib>` and `-L dependency=<dir>` for each that holds a crate (in an
//! argument file: the new layout's units outnumber what a command line holds), from which the compiler
//! finds bstd's own dependencies by their hashes. Two `libbstd-*.rlib` (an older build of
//! other features left in the same profile) are told apart by which was written last, the one
//! this test's build made or kept, and the one chosen is named in every failure.
//!
//! Each compile is a lib crate emitted as an rlib, not checked only: a refusal made by a
//! `const` assertion of an instance is a post-monomorphization error, which `--emit=metadata`
//! never reaches. Each has a deadline ([`COMPILE_MOST`]); one that passes it is killed and
//! failed, naming its file.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The longest one fixture may take to compile: each is a few lines over an already built bstd.
const COMPILE_MOST: Duration = Duration::from_secs(120);

/// The compiler that built bstd.
const RUSTC: &str = env!("BSTD_RUSTC");

/// The fixtures' directory.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/refusals");

/// Where each fixture's output goes, one directory per fixture.
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");

/// One refusal and its twin.
struct Fixture {
    name: String,
    refused: PathBuf,
    twin: PathBuf,
    says: String,
}

/// Where this test's bstd is: the rlib it was linked with, and the directories of its
/// dependencies.
struct Bstd {
    /// The file with bstd's metadata: its `.rmeta` where cargo keeps it apart, else its rlib.
    rlib: PathBuf,
    deps: Vec<PathBuf>,
}

fn fixtures() -> Vec<Fixture> {
    let mut found = Vec::new();
    let entries = fs::read_dir(FIXTURES).unwrap_or_else(|error| panic!("{FIXTURES}: {error}"));
    for entry in entries {
        let path = entry.unwrap_or_else(|error| panic!("{FIXTURES}: {error}")).path();
        let file = path.file_name().and_then(|it| it.to_str()).unwrap_or_else(|| panic!("{}: not UTF-8", path.display()));
        if file.ends_with(".twin.rs") {
            let refused = path.with_file_name(file.replace(".twin.rs", ".rs"));
            assert!(refused.exists(), "{}: a twin with no refusal beside it ({})", path.display(), refused.display());
            continue;
        }
        let Some(name) = file.strip_suffix(".rs") else {
            panic!("{}: only `<name>.rs` and `<name>.twin.rs` belong in refusals/", path.display());
        };
        let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let first = text.lines().next().unwrap_or("");
        let Some(says) = first.strip_prefix("//! refused: ") else {
            panic!("{}: its first line is not `//! refused: <text the error must contain>`", path.display());
        };
        let twin = path.with_file_name(format!("{name}.twin.rs"));
        assert!(twin.exists(), "{}: no twin {} with the refused part removed", path.display(), twin.display());
        found.push(Fixture { name: name.to_owned(), refused: path.clone(), twin, says: says.trim().to_owned() });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The directories that hold the build's crates, from where this test's binary is. cargo
/// lays a profile out one of two ways: every crate in one `deps/` beside the test (the old
/// layout), or each compiled unit in its own `build/<package>/<hash>/out/` (the build
/// directory's new layout, which the test binary is in too). Any other place is a refusal
/// naming it, never a guess.
fn crate_dirs(exe: &Path) -> Vec<PathBuf> {
    let out = exe.parent().expect("the test binary is in a directory");
    if out.file_name().is_some_and(|it| it == "deps") {
        return vec![out.to_path_buf()];
    }
    let build = out.parent().and_then(Path::parent).and_then(Path::parent);
    let Some(build) = build.filter(|it| out.file_name().is_some_and(|name| name == "out") && it.file_name().is_some_and(|name| name == "build")) else {
        panic!("{}: neither cargo's `deps/` nor its `build/<package>/<hash>/out/`", out.display());
    };
    let read = |dir: &Path| fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    let mut dirs = Vec::new();
    for package in read(build) {
        let package = package.unwrap_or_else(|error| panic!("{}: {error}", build.display())).path();
        if !package.is_dir() {
            continue;
        }
        for unit in read(&package) {
            let unit = unit.unwrap_or_else(|error| panic!("{}: {error}", package.display())).path().join("out");
            if unit.is_dir() && holds_a_crate(&unit) {
                dirs.push(unit);
            }
        }
    }
    dirs.sort();
    dirs
}

/// Whether a unit's `out/` holds a compiled crate (an `.rlib` or `.rmeta`): a build script's or
/// a binary's unit holds none, and the compiler finds nothing in it.
fn holds_a_crate(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())).any(|entry| {
        let path = entry.unwrap_or_else(|error| panic!("{}: {error}", dir.display())).path();
        path.extension().is_some_and(|it| it == "rlib" || it == "rmeta")
    })
}

fn bstd() -> Bstd {
    let exe = std::env::current_exe().expect("the test's own path");
    let deps = crate_dirs(&exe);
    let mut rlibs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for dir in &deps {
        for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())) {
            let path = entry.unwrap_or_else(|error| panic!("{}: {error}", dir.display())).path();
            let file = path.file_name().and_then(|it| it.to_str()).unwrap_or("");
            if file.starts_with("libbstd-") && file.ends_with(".rlib") {
                let written = fs::metadata(&path).and_then(|it| it.modified()).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                rlibs.push((written, path));
            }
        }
    }
    rlibs.sort();
    let Some((_, rlib)) = rlibs.pop() else {
        panic!("{}: no libbstd-*.rlib in the build's {} crate directories", exe.display(), deps.len());
    };
    // Where cargo keeps metadata out of the rlib (`-Zembed-metadata=no`), the rlib holds only a
    // stub and its `.rmeta` beside it holds the metadata: a fixture, a lib crate whose rlib links
    // nothing of bstd's, is compiled against that. In the old layout the rlib holds it whole.
    let metadata = rlib.with_extension("rmeta");
    let rlib = if metadata.exists() { metadata } else { rlib };
    Bstd { rlib, deps }
}

/// A compile started, and the file its diagnostics go to (a file, not a pipe, so a long answer
/// never blocks the compiler while it is waited for).
struct Started {
    child: Child,
    said: PathBuf,
}

/// `file` compiled as the crate `crate_name`, its output in its own scratch directory.
fn start(bstd: &Bstd, file: &Path, crate_name: &str) -> Started {
    let out = Path::new(SCRATCH).join("refusals").join(crate_name);
    fs::create_dir_all(&out).unwrap_or_else(|error| panic!("{}: {error}", out.display()));
    let said = out.join("stderr.txt");
    let stderr = fs::File::create(&said).unwrap_or_else(|error| panic!("{}: {error}", said.display()));
    let mut extern_bstd = std::ffi::OsString::from("bstd=");
    extern_bstd.push(&bstd.rlib);
    #[expect(clippy::disallowed_methods, reason = "a test of the compiler's answer runs the compiler; the directory is named below")]
    let mut command = Command::new(RUSTC);
    command
        .current_dir(&out)
        .args(["--edition=2024", "--crate-type=lib", "--emit=link", "-Cdebuginfo=0", "--crate-name", crate_name])
        .arg("--extern")
        .arg(extern_bstd);
    // The compiler finds each of bstd's dependencies by its hash among these. In the build
    // directory's layout they are one a compiled unit of the whole build, more than a command
    // line holds (`Argument list too long`), so they go in an argument file (`@<file>`, one
    // argument a line).
    let mut search = String::new();
    for dir in &bstd.deps {
        let dir = dir.to_str().unwrap_or_else(|| panic!("{}: not UTF-8, which an argument file cannot hold", dir.display()));
        writeln!(search, "-L\ndependency={dir}").unwrap();
    }
    let args = out.join("search.args");
    fs::write(&args, search).unwrap_or_else(|error| panic!("{}: {error}", args.display()));
    let mut at_args = std::ffi::OsString::from("@");
    at_args.push(&args);
    command.arg(at_args);
    command
        .arg("--out-dir")
        .arg(&out)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    let child = command.spawn().unwrap_or_else(|error| panic!("{RUSTC}: {error} (compiling {})", file.display()));
    Started { child, said }
}

/// Whether the compile passed and what it said, or `None` if it passed its deadline (then it
/// is killed).
fn finish(started: Started, until: Instant) -> Option<(bool, String)> {
    let Started { mut child, said } = started;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = fs::read_to_string(&said).unwrap_or_else(|error| panic!("{}: {error}", said.display()));
                return Some((status.success(), text));
            }
            Ok(None) if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("waiting for {RUSTC}: {error}"),
        }
    }
}

#[test]
fn each_refusal_refuses_and_its_twin_compiles() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "{FIXTURES}: no refusals");
    let bstd = bstd();
    let mut failures = String::new();
    // A few compiles at a time: each loads bstd's metadata, and many at once on a laptop
    // would be memory, not speed.
    let at_once = thread::available_parallelism().map_or(1, |it| it.get()).clamp(1, 4);
    for batch in fixtures.chunks(at_once) {
        let started: Vec<_> = batch
            .iter()
            .map(|it| {
                let until = Instant::now() + COMPILE_MOST;
                (it, until, start(&bstd, &it.refused, &format!("refused_{}", it.name)), start(&bstd, &it.twin, &format!("twin_{}", it.name)))
            })
            .collect();
        for (fixture, until, refused, twin) in started {
            match finish(refused, until) {
                None => writeln!(failures, "{}: no answer within {COMPILE_MOST:?}", fixture.refused.display()).unwrap(),
                Some((passed, said)) => {
                    if passed {
                        writeln!(failures, "{}: compiled; it must be refused with {:?}", fixture.refused.display(), fixture.says).unwrap();
                    } else if !said.contains(&fixture.says) {
                        writeln!(failures, "{}: refused, but without {:?}; the compiler said:\n{said}", fixture.refused.display(), fixture.says).unwrap();
                    }
                }
            }
            match finish(twin, until) {
                None => writeln!(failures, "{}: no answer within {COMPILE_MOST:?}", fixture.twin.display()).unwrap(),
                Some((false, said)) => {
                    writeln!(failures, "{}: the twin did not compile; the compiler said:\n{said}", fixture.twin.display()).unwrap();
                }
                Some(_) => {}
            }
        }
    }
    assert!(failures.is_empty(), "against {} (deps in {} directories), with {RUSTC}:\n{failures}", bstd.rlib.display(), bstd.deps.len());
}
