#!/usr/bin/env python3
"""coldbuild.py: `cargo blaze build` and `run` from nothing are blaze's own whole build, with
no `cargo build` (docs/no-rustc.md, step K, #91), under `RA_JIT_WHOLE_BUILD=1`.

    python3 coldbuild.py                 # the fixtures
    python3 coldbuild.py bench [names]   # bench.py's projects: ripgrep fd hyperfine by default
    python3 coldbuild.py bench --keep    # and what they built, with the daemon's logs, kept

The fixtures are a workspace of a program, an example and a library with a build script,
built from nothing by `cargo blaze build` (dev, then release) and `cargo blaze run --example`:
each executable has to be where cargo puts it and print what cargo's build of the same source
does, and no `cargo build` may run (cargo would leave `.fingerprint` in the target directory).
An edit to a body after the first build is built by the daemon on what it loaded. A program that reaches a function blaze cannot compile fails its build with what it is, exit
101, and leaves no executable and no cargo build behind.

`bench` does the same for bench.py's projects, the program run with bench.py's `run`
arguments, and says how long blaze's cold build took against cargo's, then its body and
signature edits (bench.py's) against cargo's incremental build of each. The edits are built
by the daemon's worker on what it loaded for the cold build. The standard library comes from
a remote `ra-jit std` made once, as a release's does.

RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it. Linux, as allrajit.py.
"""
import os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import RA_JIT, base_env

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
# What blaze cannot compile yet: `f128` (docs/no-rustc.md, step G). `kbad`'s own `main` has
# one; `kuses` reaches a library's function that has one, whose stand-in would be left in the
# executable.
HALVED = """#![feature(f128)]

#[inline(never)]
pub fn halved(x: f64) -> f64 {
    let x: f128 = std::hint::black_box(x as f128);
    (x / 2.0) as f64
}
"""
UNSUPPORTED = {
    "Cargo.toml": '[workspace]\nmembers = ["bad", "uses", "half"]\nresolver = "2"\n',
    "bad/Cargo.toml": '[package]\nname = "kbad"\nversion = "0.1.0"\nedition = "2021"\n',
    "bad/src/main.rs": HALVED + 'fn main() {\n    println!("{}", halved(3.0));\n}\n',
    "half/Cargo.toml": '[package]\nname = "khalf"\nversion = "0.1.0"\nedition = "2021"\n',
    "half/src/lib.rs": HALVED,
    "uses/Cargo.toml": '[package]\nname = "kuses"\nversion = "0.1.0"\nedition = "2021"\n\n'
                       '[dependencies]\nkhalf = { path = "../half" }\n',
    "uses/src/main.rs": 'fn main() {\n    println!("{}", khalf::halved(3.0));\n}\n',
}


def write(root, files):
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
        with open(os.path.join(root, path), "w") as f:
            f.write(text)


def env_for(root, remote=None):
    env = base_env(os.path.join(root, "home"))
    env["RA_JIT_WHOLE_BUILD"] = "1"
    if remote:
        env["RA_JIT_STD_FROM"] = remote
    return env


def blaze(ws, env, *args):
    start = time.monotonic()
    ran = subprocess.run(["cargo", "ra-jit", *args], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    return ran, time.monotonic() - start


def cargo(ws, env, target, *args):
    """cargo's own build of the same source, into a target directory of its own."""
    env = dict(env, CARGO_TARGET_DIR=target)
    env.pop("RA_JIT_WHOLE_BUILD", None)
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


def fixtures():
    root = tempfile.mkdtemp(prefix="ra-jit-coldbuild-", dir="/tmp")
    ws, bad = os.path.join(root, "ws"), os.path.join(root, "bad")
    write(ws, FILES)
    write(bad, UNSUPPORTED)
    env = env_for(root)
    wrong = []

    def check(what, ok, said=""):
        print(f"{what:58} {'ok' if ok else 'WRONG'}", flush=True)
        if not ok:
            wrong.append(what)
            if said:
                print(said[-3000:], flush=True)

    for profile, flags in (("debug", []), ("release", ["--release"])):
        ran, took = blaze(ws, env, "build", *flags)
        said = ran.stdout + ran.stderr
        check(f"`cargo blaze build {' '.join(flags)}` from nothing builds", ran.returncode == 0, said)
        check(f"  ... no build is cargo's", "building with cargo" not in said and not cargo_built(os.path.join(ws, "target")), said)
        program = os.path.join(ws, "target", profile, "kapp")
        reference, _ = cargo(ws, env, os.path.join(root, "ref"), "build", *flags)
        assert reference.returncode == 0, reference.stderr[-3000:]
        ours = printed(program) if os.path.exists(program) else None
        check(f"  ... target/{profile}/kapp prints what cargo's does", ours == printed(os.path.join(root, "ref", profile, "kapp")),
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
    reference, _ = cargo(ws, env, os.path.join(root, "ref"), "run", "-q", "--example", "kex", "--", "a", "b", "a")
    check("`cargo blaze run --example kex` from nothing runs it", (ran.returncode, ran.stdout) == (reference.returncode, reference.stdout),
          f"blaze's: {ran.returncode} {ran.stdout!r}\n{ran.stderr}\ncargo's: {reference.stdout!r}")
    check("  ... and it is in target/debug/examples", os.path.exists(os.path.join(ws, "target", "debug", "examples", "kex")))

    for package, whose in (("kbad", "its own"), ("kuses", "a library's")):
        ran, _ = blaze(bad, env, "build", "-p", package)
        said = ran.stdout + ran.stderr
        check(f"a program with {whose} function blaze cannot compile fails", ran.returncode == 101, said)
        check("  ... and says which and why", "could not compile" in said and "halved" in said and "f128" in said, said)
        check("  ... with no executable and no build of cargo's",
              not os.path.exists(os.path.join(bad, "target", "debug", package)) and not cargo_built(os.path.join(bad, "target")))
    subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    if wrong:
        sys.exit(f"coldbuild.py: {len(wrong)} wrong; under {root}")
    print("coldbuild.py: all right")
    shutil.rmtree(root, ignore_errors=True)


# The edits of bench.py's projects a whole build is measured on, after its cold build: the
# daemon's worker builds them on what it loaded and links the program again.
EDITS = ("body", "signature")


def edited(ws, root, name, env, build, changes, spec):
    """How long blaze's build of the edit `changes` took, then cargo's, and what was wrong with
    blaze's, if anything. Each side builds the source as it was again after."""
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
        debug = os.path.join(ws, "target", "debug")
        reference = os.path.join(root, f"{name}-ref", "debug")
        bins = [b for b in os.listdir(debug) if os.path.isfile(os.path.join(debug, b)) and os.access(os.path.join(debug, b), os.X_OK)]
        if not all(printed(os.path.join(debug, b), spec["run"], ws, env) == printed(os.path.join(reference, b), spec["run"], ws, env)
                   for b in bins):
            why = "OUTPUT DIFFERS"
    target = os.path.join(root, f"{name}-ref")
    _, cargo_took = cargo(ws, env, target, "build", *build)
    benchpy.write_edit(ws, changes, True)
    blaze(ws, env, "build", *build)
    cargo(ws, env, target, "build", *build)
    return took, cargo_took, why


def bench(names, keep=False):
    import bench as benchpy
    root = tempfile.mkdtemp(prefix="ra-jit-coldbuild-bench-", dir="/tmp")
    remote, made = benchpy.shipped_std(RA_JIT, "cranelift", root)
    rows, wrong = [], 0
    for name in names:
        spec = benchpy.PROJECTS[name]
        ws = os.path.join(root, name)
        shutil.copytree(cargo_ra_jit.fetch(name, spec), ws, symlinks=True,
                        ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
        env = dict(env_for(os.path.join(root, f"{name}-h"), remote), **spec.get("env", {}))
        subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
        build = spec.get("build", [])
        reference, cargo_took = cargo(ws, env, os.path.join(root, f"{name}-ref"), "build", *build)
        ours, took = blaze(ws, env, "build", *build)
        said = ours.stdout + ours.stderr
        verdict = "builds"
        if reference.returncode != 0:
            verdict = "cargo's build fails"
        elif ours.returncode != 0:
            verdict = "FAILS: " + (said.strip().splitlines() or [""])[-1][:160]
        elif "building with cargo" in said or cargo_built(os.path.join(ws, "target")):
            verdict = "CARGO BUILT IT"
        elif spec.get("run") is not None:
            debug = os.path.join(ws, "target", "debug")
            bins = [b for b in os.listdir(debug) if os.path.isfile(os.path.join(debug, b)) and os.access(os.path.join(debug, b), os.X_OK)]
            same = all(printed(os.path.join(debug, b), spec["run"], ws, env)
                       == printed(os.path.join(root, f"{name}-ref", "debug", b), spec["run"], ws, env) for b in bins)
            verdict = "same output" if same and bins else "OUTPUT DIFFERS"
        edits = ""
        if verdict in ("same output", "builds"):
            for kind in EDITS:
                changes = spec.get("edits", {}).get(kind)
                if changes is None:
                    continue
                ours_took, cargos_took, why = edited(ws, root, name, env, build, changes, spec)
                edits += f"  {kind} blaze {ours_took:6.2f}s cargo {cargos_took:6.2f}s"
                if why:
                    verdict = f"{kind} edit {why}"
                    break
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        wrong += verdict not in ("same output", "builds", "cargo's build fails")
        rows.append((name, verdict, took, cargo_took))
        print(f"{name:12} {verdict:40} blaze {took:7.1f}s  cargo {cargo_took:7.1f}s{edits}", flush=True)
    print(f"standard library made once in {made:.1f}s; under {root}")
    if wrong:
        sys.exit(1)
    if not keep:
        shutil.rmtree(root, ignore_errors=True)


def main():
    if not shutil.which("cargo-ra-jit", path=os.path.dirname(RA_JIT)):
        sys.exit(f"no cargo-ra-jit beside {RA_JIT}")
    if sys.argv[1:2] == ["bench"]:
        names = [it for it in sys.argv[2:] if it != "--keep"]
        bench(names or ["ripgrep", "fd", "hyperfine"], "--keep" in sys.argv)
    else:
        fixtures()


if __name__ == "__main__":
    main()
