#!/usr/bin/env python3
"""coldbuild.py: `blaze build` and `run` from nothing are blaze's own whole build, with
no `cargo build` (docs/no-rustc.md, step K, #91).

    python3 coldbuild.py                 # the fixtures
    python3 coldbuild.py statics         # the fixture of a program's own writable statics alone
    python3 coldbuild.py bench [names]   # bench.py's projects: ripgrep fd hyperfine by default
    python3 coldbuild.py bench --keep    # and what they built, with the daemon's logs, kept

The fixtures are a workspace of a program, an example and a library with a build script,
built from nothing by `blaze build` (dev, then release) and `blaze run --example`:
each executable has to be where cargo puts it and print what its source says (`KAPP_PRINT`,
`KEX_PRINT`), and no `cargo build` may run (cargo would leave `.fingerprint` in the target
directory). An edit to a body after the first build is built by the daemon on what it loaded. A program that reaches a function blaze cannot compile fails its build with what it is, exit
101, and leaves no executable and no cargo build behind. A program whose own crates have writable
statics (`OnceLock`, `Mutex`, atomics, `static mut`) that `main` reaches is run by
`blaze run`, dev and `--release`, and has to print what it should (`STATICS_PRINT`): blaze
defines those statics itself, with no rustc symbol behind them.

`bench` does the same for bench.py's projects: each program is run with bench.py's `run`
arguments and must not be killed by a signal or panic out of `main` (exit 101). It says how
long blaze's cold build took against cargo's, the speed baseline, then its body and signature
edits (bench.py's) against cargo's incremental build of each. The edits are built by the
daemon's worker on what it loaded for the cold build, and each program has to print what
blaze's build of the edited source from nothing prints (`blaze_cli.Scratch`). The standard
library's crates are units of each project's cold build like any other.

BLAZE_BIN names the blaze built; blaze is found next to it. Linux; `statics` on macOS too.
"""
import os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import blaze_cli
from blaze_cli import BLAZE, base_env

TIMEOUT = 3600
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "parts"]\nresolver = "2"\n',
    "parts/Cargo.toml": '[package]\nname = "kparts"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n',
    "parts/build.rs": 'fn main() {\n    println!("cargo:rustc-env=KPARTS_SAID=from the build script");\n}\n',
    "parts/src/lib.rs": """use std::collections::BTreeMap;

pub fn said() -> &'static str {
    env!("KPARTS_SAID")
}

pub fn counted(words: &str) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for word in words.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}
""",
    "app/Cargo.toml": '[package]\nname = "kapp"\nversion = "0.3.1"\nedition = "2021"\n\n'
                      '[dependencies]\nkparts = { path = "../parts" }\n',
    "app/src/main.rs": """fn main() {
    let caught = std::panic::catch_unwind(|| {
        let v: Vec<u8> = Vec::new();
        v[1]
    });
    println!("{} {:?}", kparts::said(), kparts::counted("a b a c a"));
    println!("caught {} in {} v{}", caught.is_err(), env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    if cfg!(debug_assertions) { println!("debug") } else { println!("release") }
}
""",
    "app/examples/kex.rs": """fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("kex {:?} {}", args, kparts::counted(&args.join(" ")).len());
}
""",
}
# What blaze cannot compile on any target: `do yeet` (`yeet_expr`), which rust-analyzer's MIR
# lowering refuses. Not a class of no-rustc step E, whose lanes make the others compile (`f128`
# was this until it compiled on AArch64 and x86-64). `kbad`'s own `main` reaches one; `kuses`
# reaches a library's function that has one, whose stand-in would be left in the executable.
HALVED = """#![feature(yeet_expr)]

#[inline(never)]
pub fn halved(x: f64) -> Option<f64> {
    if std::hint::black_box(x) < 0.0 {
        do yeet;
    }
    Some(x / 2.0)
}
"""
UNSUPPORTED = {
    "Cargo.toml": '[workspace]\nmembers = ["bad", "uses", "half"]\nresolver = "2"\n',
    "bad/Cargo.toml": '[package]\nname = "kbad"\nversion = "0.1.0"\nedition = "2021"\n',
    "bad/src/main.rs": HALVED + 'fn main() {\n    println!("{:?}", halved(3.0));\n}\n',
    "half/Cargo.toml": '[package]\nname = "khalf"\nversion = "0.1.0"\nedition = "2021"\n',
    "half/src/lib.rs": HALVED,
    "uses/Cargo.toml": '[package]\nname = "kuses"\nversion = "0.1.0"\nedition = "2021"\n\n'
                       '[dependencies]\nkhalf = { path = "../half" }\n',
    "uses/src/main.rs": 'fn main() {\n    println!("{:?}", khalf::halved(3.0));\n}\n',
}
# Writable statics of the program's own crates, reached from `main`: anim's `HOME`, a `OnceLock`
# set in the `detect` that `main` calls, which a build refused as "writable static `HOME`
# without a rustc symbol"; a `Mutex`, atomics (one in a function's body), a `static mut`, and a
# library's. A whole build has no rustc copy of any of them: blaze defines each.
STATICS = {
    "Cargo.toml": '[workspace]\nmembers = ["kstatics", "kstate"]\nresolver = "2"\n',
    "kstate/Cargo.toml": '[package]\nname = "kstate"\nversion = "0.1.0"\nedition = "2024"\n',
    "kstate/src/lib.rs": """use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

pub static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub fn bump() -> u32 {
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}
""",
    "kstatics/Cargo.toml": '[package]\nname = "kstatics"\nversion = "0.1.0"\nedition = "2024"\n\n'
                           '[dependencies]\nkstate = { path = "../kstate" }\n',
    "kstatics/src/main.rs": """use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

struct Caps {
    rows: u16,
}

fn detect(answer: &str) -> std::io::Result<Caps> {
    let mut parts = answer.split(';').map(|it| it.parse::<u16>());
    if let (Some(Ok(row)), Some(Ok(col))) = (parts.next(), parts.next()) {
        let _ = HOME.set((row, col));
    }
    Ok(Caps { rows: 24 })
}

static HOME: OnceLock<(u16, u16)> = OnceLock::new();
static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CALLS: AtomicU32 = AtomicU32::new(0);
static mut TOTAL: u64 = 0;

fn note(what: String) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    SEEN.lock().unwrap().push(what);
}

fn tally(n: u64) {
    unsafe { TOTAL += n };
}

fn frame() -> u64 {
    static FRAME: AtomicU64 = AtomicU64::new(0);
    FRAME.fetch_add(1, Ordering::Relaxed)
}

fn main() {
    let caps = detect(std::hint::black_box("12;40")).unwrap();
    let threads: Vec<_> = (0..4).map(|i| std::thread::spawn(move || note(format!("t{i}")))).collect();
    for thread in threads {
        thread.join().unwrap();
    }
    note("main".to_owned());
    for n in 1..=10 {
        tally(n);
    }
    kstate::NAMES.lock().unwrap().extend(["a", "b"]);
    let mut seen = SEEN.lock().unwrap().clone();
    seen.sort();
    println!("home {:?} rows {}", HOME.get(), caps.rows);
    println!("calls {} seen {:?} total {}", CALLS.load(Ordering::Relaxed), seen, unsafe { TOTAL });
    println!("frames {} {} {}", frame(), frame(), frame());
    println!("kstate {:?} {} {}", kstate::NAMES.lock().unwrap(), kstate::bump(), kstate::bump());
}
""",
}
STATICS_PRINT = """home Some((12, 40)) rows 24
calls 5 seen ["main", "t0", "t1", "t2", "t3"] total 55
frames 0 1 2
kstate ["a", "b"] 1 2
"""
# kapp's stdout by profile, and what its caught panic says on stderr.
KAPP_PRINT = 'from the build script {"a": 3, "b": 1, "c": 1}\ncaught true in kapp v0.3.1\n%s\n'
KAPP_PANIC = "index out of bounds: the len is 0 but the index is 1"
KEX_PRINT = 'kex ["a", "b", "a"] 2\n'


def write(root, files):
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
        with open(os.path.join(root, path), "w") as f:
            f.write(text)


def env_for(root):
    return base_env(os.path.join(root, "home"))


def blaze(ws, env, *args):
    start = time.monotonic()
    ran = subprocess.run(["blaze", *args], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    return ran, time.monotonic() - start


def cargo(ws, env, target, *args):
    """cargo's own build of the same source, into a target directory of its own: the speed
    baseline of `bench`."""
    env = dict(env, CARGO_TARGET_DIR=target)
    start = time.monotonic()
    ran = subprocess.run(["cargo", *args], cwd=ws, env=env, capture_output=True, text=True, timeout=TIMEOUT)
    return ran, time.monotonic() - start


def printed(program, args=(), cwd=None, env=None):
    """How `program` exits and what it prints, thread ids aside."""
    ran = subprocess.run([program, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=600)
    return ran.returncode, ran.stdout, re.sub(r"thread '([^']*)' \(\d+\)", r"thread '\1'", ran.stderr)


def cargo_built(target):
    """Whether cargo built anything into `target`: it leaves `.fingerprint` in each profile in
    its first layout of the build directory, and `fingerprint` in each unit's directory,
    `build/<package>/<hash>`, in its second. The build scripts and proc macros blaze loads the
    workspace with are in its own `target/rust-analyzer`."""
    own = os.path.join(target, "rust-analyzer")
    unit = lambda at: (re.fullmatch(r"[0-9a-f]{16}", os.path.basename(at))
                       and os.path.basename(os.path.dirname(os.path.dirname(at))) == "build")
    return any(".fingerprint" in dirs or ("fingerprint" in dirs and unit(at))
               for at, dirs, _ in os.walk(target)
               if at != own and not at.startswith(own + os.sep))


def statics(root, env, check):
    """`blaze run` of STATICS, dev and release: it prints STATICS_PRINT."""
    ws = os.path.join(root, "statics")
    write(ws, STATICS)
    for flags in ([], ["--release"]):
        ran, _ = blaze(ws, env, "run", "-q", *flags)
        said = ran.stdout + ran.stderr
        named = " ".join(["`blaze run", *flags]) + "`"
        check(f"{named} of a program's own writable statics runs", (ran.returncode, ran.stdout) == (0, STATICS_PRINT),
              f"blaze's: exit {ran.returncode} {ran.stdout!r}\n{ran.stderr}")
        check("  ... no build is cargo's", "building with cargo" not in said and not cargo_built(os.path.join(ws, "target")), said)
    return ws


def fixtures(only_statics=False):
    root = tempfile.mkdtemp(prefix="blaze-coldbuild-", dir="/tmp")
    env = env_for(root)
    wrong = []

    def check(what, ok, said=""):
        print(f"{what:58} {'ok' if ok else 'WRONG'}", flush=True)
        if not ok:
            wrong.append(what)
            if said:
                print(said[-3000:], flush=True)

    ws = statics(root, env, check)
    if not only_statics:
        whole(root, env, check)
    subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    if wrong:
        sys.exit(f"coldbuild.py: {len(wrong)} wrong; under {root}")
    print("coldbuild.py: all right")
    shutil.rmtree(root, ignore_errors=True)


def whole(root, env, check):
    """The fixtures of FILES and UNSUPPORTED."""
    ws, bad = os.path.join(root, "ws"), os.path.join(root, "bad")
    write(ws, FILES)
    write(bad, UNSUPPORTED)
    for profile, flags in (("debug", []), ("release", ["--release"])):
        ran, took = blaze(ws, env, "build", *flags)
        said = ran.stdout + ran.stderr
        check(f"`blaze build {' '.join(flags)}` from nothing builds", ran.returncode == 0, said)
        check(f"  ... no build is cargo's", "building with cargo" not in said and not cargo_built(os.path.join(ws, "target")), said)
        program = os.path.join(ws, "target", profile, "kapp")
        ours = printed(program) if os.path.exists(program) else None
        check(f"  ... target/{profile}/kapp prints what its source says",
              ours is not None and ours[:2] == (0, KAPP_PRINT % profile) and KAPP_PANIC in ours[2],
              f"blaze's printed {ours!r}")
        if profile == "debug":
            # The daemon's worker has the workspace loaded: an edit is built on what it has.
            main = os.path.join(ws, "app", "src", "main.rs")
            with open(main) as f:
                source = f.read()
            with open(main, "w") as f:
                f.write(source.replace('println!("debug")', 'println!("debug, edited")'))
            ran, again = blaze(ws, env, "build")
            said = ran.stdout + ran.stderr
            check("  ... an edit to a body then builds", ran.returncode == 0, said)
            check("  ... on what the daemon has loaded", "building with cargo" not in said and not cargo_built(os.path.join(ws, "target"))
                  and again < took, f"{said}\nfrom nothing {took:.1f}s, the edit {again:.1f}s")
            edited = printed(program) if os.path.exists(program) else None
            check("  ... and target/debug/kapp prints the edit", edited is not None and "debug, edited" in edited[1],
                  f"blaze's printed {edited!r}")
            with open(main, "w") as f:
                f.write(source)
    shutil.rmtree(os.path.join(ws, "target"))
    ran, _ = blaze(ws, env, "run", "-q", "--example", "kex", "--", "a", "b", "a")
    check("`blaze run --example kex` from nothing runs it", (ran.returncode, ran.stdout) == (0, KEX_PRINT),
          f"blaze's: {ran.returncode} {ran.stdout!r}\n{ran.stderr}")
    check("  ... and it is in target/debug/examples", os.path.exists(os.path.join(ws, "target", "debug", "examples", "kex")))

    for package, whose in (("kbad", "its own"), ("kuses", "a library's")):
        ran, _ = blaze(bad, env, "build", "-p", package)
        said = ran.stdout + ran.stderr
        check(f"a program with {whose} function blaze cannot compile fails", ran.returncode == 101, said)
        check("  ... and says which and why", "could not compile" in said and "halved" in said and "yeet" in said, said)
        check("  ... with no executable and no build of cargo's",
              not os.path.exists(os.path.join(bad, "target", "debug", package)) and not cargo_built(os.path.join(bad, "target")))


# The edits of bench.py's projects a whole build is measured on, after its cold build: the
# daemon's worker builds them on what it loaded and links the program again.
EDITS = ("body", "signature")


def programs(debug):
    """The executables of a profile's directory."""
    return [b for b in os.listdir(debug) if os.path.isfile(os.path.join(debug, b)) and os.access(os.path.join(debug, b), os.X_OK)]


def edited(ws, root, name, env, build, changes, spec):
    """How long blaze's build of the edit `changes` took, then cargo's, and what was wrong with
    blaze's, if anything: its programs have to print what blaze's build of the edited source
    from nothing does. Each side builds the source as it was again after."""
    import bench as benchpy
    benchpy.write_edit(ws, changes, False)
    ours, took = blaze(ws, env, "build", *build)
    said = ours.stdout + ours.stderr
    why = None
    if ours.returncode != 0:
        why = "FAILS: " + (said.strip().splitlines() or [""])[-1][:160]
    elif "building with cargo" in said or cargo_built(os.path.join(ws, "target")):
        why = "CARGO BUILT IT"
    elif spec.get("run") is not None:
        _, fresh = blaze_cli.Scratch(ws, env).cargo("build", *build, timeout=TIMEOUT)
        debug, scratch = os.path.join(ws, "target", "debug"), os.path.join(ws, "target-scratch", "debug")
        if fresh.returncode != 0:
            why = "FAILS FROM NOTHING: " + (fresh.stderr.strip().splitlines() or [""])[-1][:160]
        elif not all(printed(os.path.join(debug, b), spec["run"], ws, env) == printed(os.path.join(scratch, b), spec["run"], ws, env)
                     for b in programs(debug)):
            why = "OUTPUT DIFFERS FROM NOTHING"
        shutil.rmtree(os.path.join(ws, "target-scratch"), ignore_errors=True)
    target = os.path.join(root, f"{name}-ref")
    _, cargo_took = cargo(ws, env, target, "build", *build)
    benchpy.write_edit(ws, changes, True)
    blaze(ws, env, "build", *build)
    cargo(ws, env, target, "build", *build)
    return took, cargo_took, why


def bench(names, keep=False):
    import bench as benchpy
    root = tempfile.mkdtemp(prefix="blaze-coldbuild-bench-", dir="/tmp")
    rows, wrong = [], 0
    for name in names:
        spec = benchpy.PROJECTS[name]
        ws = os.path.join(root, name)
        shutil.copytree(blaze_cli.fetch(name, spec), ws, symlinks=True,
                        ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
        env = dict(env_for(os.path.join(root, f"{name}-h")), **spec.get("env", {}))
        subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
        build = spec.get("build", [])
        reference, cargo_took = cargo(ws, env, os.path.join(root, f"{name}-ref"), "build", *build)
        ours, took = blaze(ws, env, "build", *build)
        said = ours.stdout + ours.stderr
        verdict = "builds"
        if ours.returncode != 0:
            verdict = "FAILS: " + (said.strip().splitlines() or [""])[-1][:160]
        elif "building with cargo" in said or cargo_built(os.path.join(ws, "target")):
            verdict = "CARGO BUILT IT"
        elif spec.get("run") is not None:
            debug = os.path.join(ws, "target", "debug")
            bins = programs(debug)
            ended = [printed(os.path.join(debug, b), spec["run"], ws, env)[0] for b in bins]
            crashed = [f"{b} exited {code}" for b, code in zip(bins, ended) if code < 0 or code == 101]
            verdict = ("CRASHES: " + ", ".join(crashed)[:160]) if crashed else "runs" if bins else "NO PROGRAM"
        if reference.returncode != 0:
            # The baseline is gone, blaze's build is not: it is judged all the same.
            verdict += "; cargo's build fails"
        edits = ""
        if verdict.startswith(("runs", "builds")):
            for kind in EDITS:
                changes = spec.get("edits", {}).get(kind)
                if changes is None:
                    continue
                ours_took, cargos_took, why = edited(ws, root, name, env, build, changes, spec)
                edits += f"  {kind} blaze {ours_took:6.2f}s cargo {cargos_took:6.2f}s"
                if why:
                    verdict = f"{kind} edit {why}"
                    break
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        wrong += not verdict.startswith(("runs", "builds"))
        rows.append((name, verdict, took, cargo_took))
        print(f"{name:12} {verdict:40} blaze {took:7.1f}s  cargo {cargo_took:7.1f}s{edits}", flush=True)
    print(f"under {root}")
    if wrong:
        sys.exit(1)
    if not keep:
        shutil.rmtree(root, ignore_errors=True)


def main():
    if not shutil.which("blaze", path=os.path.dirname(BLAZE)):
        sys.exit(f"no blaze beside {BLAZE}")
    if sys.argv[1:2] == ["bench"]:
        names = [it for it in sys.argv[2:] if it != "--keep"]
        bench(names or ["ripgrep", "fd", "hyperfine"], "--keep" in sys.argv)
    else:
        fixtures(only_statics=sys.argv[1:2] == ["statics"])


if __name__ == "__main__":
    main()
