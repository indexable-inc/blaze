"""One tree, one build, one table: merge every lane branch into an integration worktree, build once
with cargo, and run every check against that one binary. Input: one JSON value (`Input`):

    {"branches": [...], "tests": true|false,
     "named": [{"package": "blaze-engine", "args": ["--test", "revisions"], "tests": ["a::test"], "runs": 6}]}

A row counts only with proof that the thing ran (ran.py, rule claims-from-evidence): every timing
goes through `ran.run` or `ran.series` (the fixed environment, the exit status, and the text the
program must print), every test run through `ran.tests` (how many tests it is expected to have
run), and every git or build step is held to its exit status. A row without its proof is printed
as `INVALID: <why>`, never as a number, and the script then exits 1. `named` reruns the tests it
names, each run expected to run exactly those: a name that matches no test is INVALID, not a pass
(agents.json A39).
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

# Run with `python3 -I`, which leaves the script's directory off the path: ran.py is beside it.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ran
from ran import Command, Proof, Terminal, Tests


@dataclass(frozen=True)
class Named:
    """Tests rerun by name: `cargo test -p <package> <args> -- --exact <tests>`, `runs` times."""
    package: str
    tests: tuple[str, ...]
    args: tuple[str, ...] = ()
    runs: int = 1


@dataclass(frozen=True)
class Input:
    branches: tuple[str, ...]
    tests: bool = False
    named: tuple[Named, ...] = ()


if len(sys.argv) != 2:
    sys.exit("usage: integrate.py '<one JSON object>' (see the top of the file)")
try:
    IN = ran.parsed(Input, json.loads(sys.argv[1]))
except (json.JSONDecodeError, ValueError) as e:
    sys.exit(f"integrate.py: {e}")
REPO = Path("/Volumes/Projects/blaze")
WT = Path("/Volumes/Projects/blaze-int-1112")
INST = Path("/tmp/blaze-int")
HOME_ = Path("/private/tmp/blaze-home-int")
TARGET = "/tmp/blaze-cargo-target"
LLVM = os.path.expanduser("~/.blaze/native/sha256-e95fc3b4bd332691ec96b022850b5e5b8684b4a2060c05c31b481c31f0f1a866")
RELAX = "/tmp/relax-src/relax/target/release/relax"
RH = "/private/tmp/relax-hello/hello"
ENV = {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "BLAZE_HOME": str(HOME_), "TMPDIR": os.environ.get("TMPDIR", "/tmp")}
ROWS: list[tuple[str, str]] = []
# The rows that are no result: INVALID, FAILED, CONFLICT, TIMED OUT. Any makes the exit status 1.
BAD: list[str] = []


def sh(args: list[str], cwd: Path | str = REPO, env: dict[str, str] | None = None, timeout: int = 3600) -> subprocess.CompletedProcess[str]:
    # check=False: each caller holds the exit status itself (`must`, or a row that says it).
    return subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout, check=False)


def row(k: str, v: str, ok: bool = True) -> None:
    """One row. `ok` False, or a value that says it is no result, counts against the run."""
    ROWS.append((k, v))
    if not ok or v.startswith(("INVALID", "FAILED", "CONFLICT", "TIMED OUT")):
        BAD.append(k)
    print(f"{k:44s} {v}", flush=True)


def git(*a: str) -> subprocess.CompletedProcess[str]:
    return sh(["git", *a], cwd=WT)


def must(r: subprocess.CompletedProcess[str], what: str) -> subprocess.CompletedProcess[str]:
    """`r`, which has to have worked: a step that failed stops the run, naming the step."""
    if r.returncode != 0:
        sys.exit(f"integrate.py: {what} failed (exit {r.returncode}): {' '.join(map(str, r.args))}\n{r.stderr[-2000:]}")
    return r


def blaze(*args: str, cwd: str, terminal: Terminal | None = None) -> Command:
    return Command((str(INST / "blaze"), *args), cwd, ENV, terminal=terminal)


def relax(*args: str) -> Command:
    return Command((RELAX, *args), RH, ENV)


def merge() -> None:
    must(sh(["git", "fetch", "-q", "origin", "--prune"]), "the fetch")
    if not WT.exists():
        must(sh(["git", "worktree", "add", "-q", "-B", "int-1112", str(WT), "origin/main"]), "adding the integration worktree")
    else:
        assert must(git("status", "--porcelain"), "git status").stdout.strip() == "", "integration worktree is dirty"
        must(git("merge", "-q", "--no-edit", "origin/main"), "merging origin/main into the integration worktree")
    for b in IN.branches:
        r = git("merge", "--no-edit", f"origin/{b}")
        if r.returncode == 0:
            row(f"merge {b}", "ok " + must(git("rev-parse", "--short", f"origin/{b}"), "git rev-parse").stdout.strip())
            continue
        bad = [l[3:] for l in git("status", "--porcelain").stdout.splitlines() if l[:2] in ("UU", "AA", "DU", "UD", "AU", "UA")]
        if bad and set(bad) <= {"annoyances.json", "agents.json"}:
            # main keeps one file per row under agents/; a lane that began earlier still appends to
            # one of the old files: its rows become files and the old file stays deleted.
            for old in bad:
                theirs = must(git("show", f"origin/{b}:{old}"), f"reading {old} of origin/{b}").stdout
                tmp = Path(f"/tmp/integrate-{b}-{old}")
                tmp.write_text(theirs)
                must(sh(["python3", "-I", "harness/v2/agents.py", json.dumps({"import": str(tmp), "lane": b})], cwd=WT),
                     f"importing the rows of {old} of {b} (harness/v2/agents.py)")
                must(git("rm", "-q", "--cached", old), "git rm")
                (WT / old).unlink(missing_ok=True)
            must(git("add", "agents"), "git add")
            must(git("commit", "-q", "--no-edit"), f"committing the merge of {b}")
            row(f"merge {b}", "ok (its rows written under agents/)")
        else:
            must(git("merge", "--abort"), f"aborting the merge of {b}")
            row(f"merge {b}", "CONFLICT, left out: " + (", ".join(bad)[:150] or r.stderr.strip()[-150:]))
    row("tree", must(git("rev-parse", "--short", "HEAD"), "git rev-parse").stdout.strip())


def build() -> bool:
    env = dict(os.environ, LLVM_SYS_231_PREFIX=LLVM, CARGO_TARGET_DIR=TARGET)
    s = time.time()
    r = sh(["cargo", "+nightly-2026-10-27", "build", "--release", "-p", "blaze-thin", "-p", "blaze-engine"], cwd=WT, env=env)
    if r.returncode != 0:
        errs = [l for l in r.stderr.splitlines() if l.startswith("error")]
        row("build", f"FAILED (exit {r.returncode}), {len(errs)} errors")
        print("\n".join(r.stderr.splitlines()[-60:]))
        return False
    # The build's proof: both binaries, written by this build or kept by it (cargo's exit says which).
    built = [Path(TARGET, "release", name) for name in ("blaze-thin", "blaze-engine")]
    missing = [str(it) for it in built if not it.is_file()]
    if missing:
        row("build", "INVALID: cargo exited 0 and did not leave " + ", ".join(missing))
        return False
    row("build", f"ok {time.time() - s:.0f} s")
    if must(git("status", "--porcelain"), "git status").stdout.strip() == "M Cargo.lock":
        must(git("commit", "-qam", "Cargo.lock as the combined build left it"), "committing Cargo.lock")
    if (INST / "blaze").exists():
        # The last run's daemon: stopped by the binary that started it, and said when that fails
        # (its home is removed below either way; a daemon left running holds memory, not the row).
        r = sh([str(INST / "blaze"), "stop"], cwd="/", env=ENV, timeout=120)
        if r.returncode != 0:
            print(f"the last run's daemon did not stop (exit {r.returncode}): {ran.tail(r.stderr)}", flush=True)
    shutil.rmtree(INST, ignore_errors=True)
    INST.mkdir()
    shutil.copy(f"{TARGET}/release/blaze-thin", INST / "blaze")
    shutil.copy(f"{TARGET}/release/blaze-engine", INST / "blaze-engine")
    shutil.rmtree(HOME_, ignore_errors=True)
    return True


def once(label: str, command: Command, proof: Proof) -> ran.Ran:
    """One timed run as a row of its own: its time with its exit and proof checked, or INVALID."""
    r = ran.run(command, proof)
    row(label, f"INVALID: {r.invalid}" if r.invalid else f"{r.ms:8.0f} ms  exit {r.exit}, proof checked")
    return r


def series(label: str, command: Command, proof: Proof, n: int, edit: Callable[[int], None] | None = None, warm: int = 0,
           proof_of: Callable[[int], Proof] | None = None) -> None:
    row(label, ran.series(label, command, proof, n, warm, edit, proof_of).text)


def new_project(d: str, name: str, value: int) -> None:
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d + "/src")
    Path(d, "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n')
    Path(d, "src/main.rs").write_text(f'fn add(a: u64) -> u64 {{\n    a + {value})\n}}\nfn main() {{\n    println!("hello: {{}}", add(1));\n}}\n'.replace(f"a + {value})", f"(a + {value})"))


def body(p: str):
    def f(i: int) -> None:
        s = Path(p).read_text()
        Path(p).write_text(re.sub(r"\+ \d+\)", f"+ {300 + i})", s, count=1))
    return f


def comment(p: str):
    def f(i: int) -> None:
        with open(p, "a") as h:
            h.write(f"// d{i}\n")
    return f


def daemons() -> list[tuple[int, int]]:
    """The daemons of the binary under test that run now: each pid and its resident kilobytes."""
    found: list[tuple[int, int]] = []
    for pid in sh(["pgrep", "-f", str(INST / "blaze-engine")]).stdout.split():
        o = sh(["ps", "-o", "rss=,command=", "-p", pid]).stdout
        if " serve" in o:
            found.append((int(pid), int(o.split()[0])))
    return found


def measure() -> None:
    A = "/private/tmp/int-hello-a"
    new_project(A, "int-hello-a", 1)
    main_rs = A + "/src/main.rs"
    hello = Proof(stdout="hello")
    # The program prints `hello: <the edit's number + 1>`: the proof that this edit's code ran.
    def says(value: int) -> Proof:
        return Proof(stdout=f"hello: {value + 1}")
    r = once("cold first run (fresh home)", blaze("run", "-q", cwd=A), says(1))
    if r.invalid:
        print(r.stderr[-3000:])
        return
    series("client start (--version)", blaze("--version", cwd=A), Proof(stdout="0."), 30)
    series("relax --version", relax("--version"), Proof(stdout="."), 30)
    # Not `-q`: the `blaze: ok built=0` line is the proof that blaze answered and compiled nothing.
    series("no-op build", blaze("build", cwd=A), Proof(stderr="blaze: ok built=0 "), 30, warm=2)
    series("no-op run", blaze("run", "-q", cwd=A), says(1), 30, warm=2)
    series("relax no-op run", relax("run", "--quiet", "hello"), hello, 30, warm=2)
    series("body edit + run", blaze("run", "-q", cwd=A), hello, 20, edit=body(main_rs), proof_of=lambda i: says(300 + i))
    series("relax body edit + run", relax("run", "--quiet", "hello"), hello, 20, edit=body(RH + "/src/main.rs"))
    series("comment edit + run", blaze("run", "-q", cwd=A), says(319), 10, edit=comment(main_rs))
    series("relax comment edit + run", relax("run", "--quiet", "hello"), hello, 10, edit=comment(RH + "/src/main.rs"))
    # a save by rename
    def rename(i: int) -> None:
        text = re.sub(r"\+ \d+\)", f"+ {900 + i})", Path(main_rs).read_text(), count=1)
        Path(main_rs + ".tmp").write_text(text)
        os.replace(main_rs + ".tmp", main_rs)
    series("body edit saved by rename + run", blaze("run", "-q", cwd=A), hello, 6, edit=rename, proof_of=lambda i: says(900 + i))
    # every edit's run must print that edit's number: a stale answer is a wrong program
    stale = 0
    bad = 0
    edits = 1500
    for i in range(edits):
        want = 5000 + i
        src = re.sub(r"\+ \d+\)", f"+ {want})", Path(main_rs).read_text(), count=1)
        Path(main_rs).write_text(src)
        if i % 100 == 99:
            time.sleep(3 + (i % 300) / 10)  # some edits come after an idle stretch
        r = ran.run(blaze("run", "-q", cwd=A), says(want))
        if r.exit != 0:
            bad += 1
        elif r.invalid:
            stale += 1
            print(f"STALE at edit {i}: wanted {want + 1}, got {r.stdout.strip()[-40:]}", flush=True)
    row(f"{edits} body edits: stale answers / failures", f"{stale} / {bad}, each run held to its edit's number", ok=stale == 0 and bad == 0)
    # a brand-new project in the warm daemon
    for k in range(3):
        B = f"/private/tmp/int-hello-new{k}"
        new_project(B, f"int-hello-new{k}", 7 + k)
        t = [ran.run(blaze("run", "-q", cwd=B), says(7 + k)) for _ in range(3)]
        invalid = [f"run {at}: {r.invalid}" for at, r in enumerate(t) if r.invalid]
        row(f"new project {k}: first/second/third run", "INVALID: " + invalid[0] if invalid else "  ".join(f"{r.ms:7.0f} ms" for r in t) + "  each with its exit and proof checked")
    # another terminal width must rebuild nothing
    for cols in (80, 200):
        r = ran.run(blaze("run", cwd=A, terminal=Terminal(cols=cols)), hello)
        text = r.stdout
        row(f"run in a terminal {cols} wide", f"INVALID: {r.invalid}" if r.invalid else f"{r.ms:7.0f} ms  exit {r.exit}, ran  recomputed-note={'recomputed' in text}  compiling-lines={text.count('Compiling')}",
            ok=not r.invalid and "recomputed" not in text)
    # memory and disk
    serving = daemons()
    # No daemon found is no reading of its memory: 0 MB would be a number for nothing.
    row("daemon resident memory", f"{max(rss for _, rss in serving) / 1024:8.0f} MB" if serving else f"INVALID: no `blaze-engine serve` of {INST} is running")
    for label, where in (("blaze home on disk", str(HOME_)), ("relax state on disk", RH + "/target")):
        du = sh(["du", "-sk", where])
        row(label, f"{int(du.stdout.split()[0]) / 1024:8.0f} MB" if du.returncode == 0 and du.stdout.split() else f"INVALID: du of {where} failed (exit {du.returncode}): {ran.tail(du.stderr)}")
    # restart
    if not serving:
        row("restart", "INVALID: no daemon was running to restart")
        return
    stop = sh([str(INST / "blaze"), "stop"], cwd=A, env=ENV, timeout=300)
    # `stop` answers before the daemon's process has ended (agents.json rev-3): its end is waited
    # for, for at most 60 s, and a daemon still there is no restart.
    deadline = time.monotonic() + 60
    while daemons() and time.monotonic() < deadline:
        time.sleep(0.2)
    left = daemons()
    if stop.returncode != 0 or left:
        row("restart", f"INVALID: `blaze stop` exited {stop.returncode} and left {[pid for pid, _ in left]} running: {ran.tail(stop.stdout + stop.stderr)}")
        return
    once("no-op run after a restart", blaze("run", "-q", cwd=A), hello)
    body(main_rs)(77)
    once("body edit + run after a restart", blaze("run", "-q", cwd=A), says(377))


# The unit tests the loop runs, each with the fewest tests it is expected to run: a package whose
# selection ran none (a renamed target, a filter that matches nothing, a build that failed) is
# INVALID, never `passed 0 failed 0`.
SUITES: tuple[tuple[str, tuple[str, ...], Tests], ...] = (
    ("blaze-engine", ("--test", "revisions"), Tests(at_least=1)),
    ("blaze-launch", (), Tests(at_least=1)),
    ("blaze-launch-rt", (), Tests(at_least=1)),
    ("blaze-query", ("--lib",), Tests(at_least=1)),
    ("blaze-cas", ("--lib",), Tests(at_least=1)),
    ("blaze-link", ("--lib",), Tests(at_least=1)),
    ("blaze-codegen", ("--lib",), Tests(at_least=1)),
    ("blaze-screen", (), Tests(at_least=1)),
    ("blaze-cargo", ("--lib", "cli::test::"), Tests(at_least=1, named=("cli::test::tests::a_filter_that_matches_no_test_is_refused_naming_it_and_the_nearest_tests",))),
)


def cargo_test(package: str, args: tuple[str, ...], after: tuple[str, ...], expect: Tests) -> ran.Tested:
    env = dict(os.environ, LLVM_SYS_231_PREFIX=LLVM, CARGO_TARGET_DIR=TARGET, RUST_TEST_THREADS="6")
    command = ["cargo", "+nightly-2026-10-27", "test", "--release", "--no-fail-fast", "-p", package, *args, *(["--", *after] if after else [])]
    try:
        r = sh(command, cwd=WT, env=env, timeout=2400)
    except subprocess.TimeoutExpired:
        return ran.tests("", None, expect)
    return ran.tests(r.stdout, r.returncode, expect)


def tests() -> None:
    for package, args, expect in SUITES:
        read = cargo_test(package, args, (), expect)
        row(f"test {package} {' '.join(args)}", read.text, ok=not read.invalid and read.failed == 0)


def named() -> None:
    """Each `Named` rerun: every run is expected to run exactly the tests it names."""
    for it in IN.named:
        if not it.tests or it.runs < 1:
            row(f"rerun {it.package}", "INVALID: no test is named, or no run asked for")
            continue
        expect = Tests(named=it.tests, exactly=len(it.tests))
        runs = [cargo_test(it.package, it.args, ("--exact", *it.tests), expect) for _ in range(it.runs)]
        invalid = [f"run {at}: {r.invalid}" for at, r in enumerate(runs) if r.invalid]
        passed = sum(1 for r in runs if not r.invalid and r.failed == 0)
        failed = sum(1 for r in runs if not r.invalid and r.failed > 0)
        label = f"rerun {it.package} {' '.join(it.tests)[:60]}"
        if invalid:
            row(label, f"INVALID: {len(invalid)} of {it.runs} runs do not count; {invalid[0]}")
        else:
            row(label, f"pass {passed} fail {failed} of {it.runs} runs, each of which ran its {len(it.tests)} named tests", ok=failed == 0)


def generated() -> None:
    """Each generated file is what its generator writes from this tree. A generator that failed
    and left its old output behind (a parse error read past, agents.json) shows here: each check
    is held to its exit status and to the line it prints only when it ran to its end."""
    env = dict(os.environ, LLVM_SYS_231_PREFIX=LLVM, CARGO_TARGET_DIR=TARGET)
    scripts = ("cargo", "+nightly-2026-10-27", "run", "-q", "--release", "-p", "scripts", "--bin")
    checks: tuple[tuple[str, tuple[str, ...], Proof], ...] = (
        ("AGENTS.md and the skills (rules.ts)", ("bun", "tools/agents/rules.ts", "--check"), Proof(stdout="skills are what")),
        ("docs/README.md and the docs (docs.rs)", (*scripts, "docs", "--", "check"), Proof(stdout="docs hold")),
        ("library/DIGEST (library.py)", ("uv", "run", "--frozen", "python", "harness/library.py", "check", "{}"), Proof(stdout="is the digest of library/")),
    )
    for label, args, proof in checks:
        r = ran.run(Command(args, str(WT), env, timeout=2400), proof)
        row(f"generated: {label}", f"INVALID: {r.invalid}" if r.invalid else "fresh, its check ran to its end")


merge()
generated()
if build():
    measure()
    if IN.tests:
        tests()
    named()
elif IN.tests or IN.named:
    row("tests", "INVALID: not run, the build failed")
Path("/tmp/blaze-integrate.last.json").write_text(json.dumps(ROWS, indent=1))
if BAD:
    sys.exit(f"integrate.py: {len(BAD)} of {len(ROWS)} rows are no result: " + ", ".join(BAD))
print(f"integrate.py: {len(ROWS)} rows, each with what it ran checked")
