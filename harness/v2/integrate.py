#!/usr/bin/env python3
"""One tree, one build, one table: merge every lane branch into an integration worktree, build once
with cargo, and run every check against that one binary. Input: one JSON value
{"branches": [...], "tests": true|false}."""
import json, os, re, shutil, subprocess, sys, time
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Input:
    branches: tuple[str, ...]
    tests: bool = False


RAW = json.loads(sys.argv[1])
assert set(RAW) <= {"branches", "tests"}, RAW
IN = Input(tuple(RAW["branches"]), bool(RAW.get("tests", False)))
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


def sh(args: list[str], cwd: Path | str = REPO, env: dict[str, str] | None = None, timeout: int = 3600) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def row(k: str, v: str) -> None:
    ROWS.append((k, v))
    print(f"{k:44s} {v}", flush=True)


def git(*a: str) -> subprocess.CompletedProcess[str]:
    return sh(["git", *a], cwd=WT)


def merge() -> None:
    sh(["git", "fetch", "-q", "origin", "--prune"])
    if not WT.exists():
        r = sh(["git", "worktree", "add", "-q", "-B", "int-1112", str(WT), "origin/main"])
        assert r.returncode == 0, r.stderr
    else:
        assert git("status", "--porcelain").stdout.strip() == "", "integration worktree is dirty"
        git("merge", "-q", "--no-edit", "origin/main")
    for b in IN.branches:
        r = git("merge", "--no-edit", f"origin/{b}")
        if r.returncode == 0:
            row(f"merge {b}", "ok " + git("rev-parse", "--short", f"origin/{b}").stdout.strip())
            continue
        bad = [l[3:] for l in git("status", "--porcelain").stdout.splitlines() if l[:2] in ("UU", "AA", "DU", "UD", "AU", "UA")]
        if bad == ["annoyances.json"]:
            # main renamed the file to agents.json; a lane that began earlier still adds rows to the old name
            ours = json.loads((WT / "agents.json").read_text())
            theirs = json.loads(git("show", f"origin/{b}:annoyances.json").stdout)
            have = {x["id"] for x in ours["annoyances"]}
            ours["annoyances"] += [x for x in theirs["annoyances"] if x["id"] not in have]
            (WT / "agents.json").write_text(json.dumps(ours, indent=1, ensure_ascii=False) + "\n")
            git("rm", "-q", "--cached", "annoyances.json")
            (WT / "annoyances.json").unlink(missing_ok=True)
            git("add", "agents.json")
            git("commit", "-q", "--no-edit")
            row(f"merge {b}", "ok (annoyance rows moved into agents.json)")
        elif bad == ["agents.json"]:
            ours = json.loads(git("show", ":2:agents.json").stdout)
            theirs = json.loads(git("show", ":3:agents.json").stdout)
            for k in ("howto", "annoyances"):
                have = {x["id"] for x in ours[k]}
                ours[k] += [x for x in theirs.get(k, []) if x["id"] not in have]
            (WT / "agents.json").write_text(json.dumps(ours, indent=1, ensure_ascii=False) + "\n")
            git("add", "agents.json")
            git("commit", "-q", "--no-edit")
            row(f"merge {b}", "ok (agents.json rows joined)")
        else:
            git("merge", "--abort")
            row(f"merge {b}", "CONFLICT, left out: " + ", ".join(bad)[:150])
    row("tree", git("rev-parse", "--short", "HEAD").stdout.strip())


def build() -> bool:
    env = dict(os.environ, LLVM_SYS_231_PREFIX=LLVM, CARGO_TARGET_DIR=TARGET)
    s = time.time()
    r = sh(["cargo", "+nightly-2026-10-27", "build", "--release", "-p", "blaze-thin", "-p", "blaze-engine"], cwd=WT, env=env)
    if r.returncode != 0:
        errs = [l for l in r.stderr.splitlines() if l.startswith("error")]
        row("build", f"FAILED, {len(errs)} errors")
        print("\n".join(r.stderr.splitlines()[-60:]))
        return False
    row("build", f"ok {time.time() - s:.0f} s")
    if git("status", "--porcelain").stdout.strip() == "M Cargo.lock":
        git("commit", "-qam", "Cargo.lock as the combined build left it")
    if (INST / "blaze").exists():
        sh([str(INST / "blaze"), "stop"], cwd="/", env=ENV, timeout=120)
    pid = HOME_ / "v2" / "serve.lock"
    shutil.rmtree(INST, ignore_errors=True)
    INST.mkdir()
    shutil.copy(f"{TARGET}/release/blaze-thin", INST / "blaze")
    shutil.copy(f"{TARGET}/release/blaze-engine", INST / "blaze-engine")
    shutil.rmtree(HOME_, ignore_errors=True)
    return True


def timed(args: list[str], cwd: str) -> tuple[float, subprocess.CompletedProcess[str]]:
    s = time.perf_counter()
    r = sh(args, cwd=cwd, env=ENV, timeout=900)
    return (time.perf_counter() - s) * 1000, r


def series(label: str, args: list[str], n: int, cwd: str, edit=None, warm: int = 0, want: str | None = None) -> None:
    xs: list[float] = []
    bad = 0
    for i in range(n + warm):
        if edit:
            edit(i)
        ms, r = timed(args, cwd)
        if r.returncode != 0 or (want and want not in r.stdout):
            bad += 1
        if i >= warm:
            xs.append(ms)
    xs.sort()
    row(label, f"median {xs[len(xs) // 2]:7.1f} ms  min {xs[0]:7.1f}  max {xs[-1]:8.1f}  n={len(xs)} bad={bad}")


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


def measure() -> None:
    T = str(INST / "blaze")
    A = "/private/tmp/int-hello-a"
    new_project(A, "int-hello-a", 1)
    ms, r = timed([T, "run", "-q"], A)
    row("cold first run (fresh home)", f"{ms:8.0f} ms  exit {r.returncode}  [{(r.stdout.strip().splitlines() or [r.stderr.strip()[-120:]])[-1][:60]}]")
    if r.returncode != 0:
        print(r.stderr[-3000:])
        return
    series("client start (--version)", [T, "--version"], 30, A)
    series("relax --version", [RELAX, "--version"], 30, RH)
    series("no-op build", [T, "build", "-q"], 30, A, warm=2)
    series("no-op run", [T, "run", "-q"], 30, A, warm=2, want="hello")
    series("relax no-op run", [RELAX, "run", "--quiet", "hello"], 30, RH, warm=2, want="hello")
    series("body edit + run", [T, "run", "-q"], 20, A, edit=body(A + "/src/main.rs"), want="hello")
    series("relax body edit + run", [RELAX, "run", "--quiet", "hello"], 20, RH, edit=body(RH + "/src/main.rs"), want="hello")
    series("comment edit + run", [T, "run", "-q"], 10, A, edit=comment(A + "/src/main.rs"), want="hello")
    series("relax comment edit + run", [RELAX, "run", "--quiet", "hello"], 10, RH, edit=comment(RH + "/src/main.rs"), want="hello")
    # a save by rename
    def rename(i: int) -> None:
        p = A + "/src/main.rs"
        s = re.sub(r"\+ \d+\)", f"+ {900 + i})", Path(p).read_text(), count=1)
        Path(p + ".tmp").write_text(s)
        os.replace(p + ".tmp", p)
    series("body edit saved by rename + run", [T, "run", "-q"], 6, A, edit=rename, want="hello")
    # every edit's run must print that edit's number: a stale answer is a wrong program
    stale = 0
    bad = 0
    for i in range(1500):
        want = 5000 + i
        src = re.sub(r"\+ \d+\)", f"+ {want})", Path(A + "/src/main.rs").read_text(), count=1)
        Path(A + "/src/main.rs").write_text(src)
        if i % 100 == 99:
            time.sleep(3 + (i % 300) / 10)  # some edits come after an idle stretch
        _, r = timed([T, "run", "-q"], A)
        if r.returncode != 0:
            bad += 1
        elif f"hello: {want + 1}" not in r.stdout:
            stale += 1
            print(f"STALE at edit {i}: wanted {want + 1}, got {r.stdout.strip()[-40:]}", flush=True)
    row("1500 body edits: stale answers / failures", f"{stale} / {bad}")
    # a brand-new project in the warm daemon
    for k in range(3):
        B = f"/private/tmp/int-hello-new{k}"
        new_project(B, f"int-hello-new{k}", 7 + k)
        t = [timed([T, "run", "-q"], B) for _ in range(3)]
        row(f"new project {k}: first/second/third run", "  ".join(f"{ms:7.0f} ms" for ms, _ in t) + f"  bad={sum(1 for _, r in t if r.returncode != 0 or 'hello' not in r.stdout)}")
    # another terminal width must rebuild nothing
    for cols in ("80", "200"):
        s = time.perf_counter()
        r = subprocess.run(["script", "-q", "/dev/null", "/bin/sh", "-c", f"stty cols {cols}; exec {T} run"], cwd=A, env=ENV, capture_output=True, text=True, timeout=900)
        row(f"run in a terminal {cols} wide", f"{(time.perf_counter() - s) * 1000:7.0f} ms  recomputed-note={'recomputed' in r.stdout}  compiling-lines={r.stdout.count('Compiling')}")
    # memory and disk
    pids = sh(["pgrep", "-f", str(INST / "blaze-engine")]).stdout.split()
    rss = 0
    for p in pids:
        o = sh(["ps", "-o", "rss=,command=", "-p", p]).stdout
        if " serve" in o:
            rss = max(rss, int(o.split()[0]))
    row("daemon resident memory", f"{rss / 1024:8.0f} MB")
    du = sh(["du", "-sk", str(HOME_)]).stdout.split()
    row("blaze home on disk", f"{int(du[0]) / 1024:8.0f} MB" if du else "?")
    du = sh(["du", "-sk", RH + "/target"]).stdout.split()
    row("relax state on disk", f"{int(du[0]) / 1024:8.0f} MB" if du else "?")
    # restart
    sh([T, "stop"], cwd=A, env=ENV, timeout=300)
    ms, r = timed([T, "run", "-q"], A)
    row("no-op run after a restart", f"{ms:8.0f} ms  exit {r.returncode}")
    body(A + "/src/main.rs")(77)
    ms, r = timed([T, "run", "-q"], A)
    row("body edit + run after a restart", f"{ms:8.0f} ms  exit {r.returncode}")


def tests() -> None:
    env = dict(os.environ, LLVM_SYS_231_PREFIX=LLVM, CARGO_TARGET_DIR=TARGET, RUST_TEST_THREADS="6")
    for pk, extra in (("blaze-engine", ["--test", "revisions"]), ("blaze-launch", []), ("blaze-launch-rt", []), ("blaze-query", ["--lib"]), ("blaze-cas", ["--lib"]), ("blaze-link", ["--lib"]), ("blaze-codegen", ["--lib"])):
        try:
            r = sh(["cargo", "+nightly-2026-10-27", "test", "--release", "--no-fail-fast", "-p", pk, *extra], cwd=WT, env=env, timeout=2400)
        except subprocess.TimeoutExpired:
            row(f"test {pk}", "TIMED OUT at 40 min")
            continue
        res = re.findall(r"test result: \w+\. (\d+) passed; (\d+) failed", r.stdout)
        failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", r.stdout, re.M)))
        row(f"test {pk} {' '.join(extra)}", f"passed {sum(int(a) for a, _ in res)} failed {sum(int(b) for _, b in res)}" + (" (did not compile)" if not res else "") + ("  " + ", ".join(failed)[:400] if failed else ""))


merge()
if build():
    measure()
    if IN.tests:
        tests()
Path("/tmp/blaze-integrate.last.json").write_text(json.dumps(ROWS, indent=1))
