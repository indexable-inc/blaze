#!/usr/bin/env python3
"""The rows lane sealed-1112's steps are judged by, from one blaze binary in one fresh home: cold
build, the first edit's engine work, the edit loop, idle memory, a second project in the warm
daemon, and a restart. Input: one JSON value
{"blaze": "/path/to/blaze", "work": "/dev/shm/dir", "label": "base", "census": false, "n": 30}.
Prints one row a line (`label | row | value`) and, last, the rows as one JSON line. With
"census": true it times nothing: it builds, edits once, and prints the daemon's durability census
(BLAZE_DURABILITY_CENSUS) from its log."""
import json, os, re, shutil, statistics, subprocess, sys, time
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str
    label: str
    census: bool = False
    n: int = 30


RAW = json.loads(sys.argv[1])
assert set(RAW) <= {"blaze", "work", "label", "census", "n"}, RAW
IN = Input(RAW["blaze"], RAW["work"], RAW["label"], bool(RAW.get("census", False)), int(RAW.get("n", 30)))
WORK = Path(IN.work)
HOME = WORK / "home"
ENV = {"HOME": "/root", "PATH": "/root/.cargo/bin:/run/current-system/sw/bin", "BLAZE_HOME": str(HOME)}
if IN.census:
    ENV["BLAZE_DURABILITY_CENSUS"] = "1"
ROWS: dict[str, object] = {}


def row(k: str, v: object) -> None:
    ROWS[k] = v
    print(f"{IN.label} | {k} | {v}", flush=True)


def timed(args: list[str], cwd: Path) -> tuple[float, subprocess.CompletedProcess[str]]:
    s = time.perf_counter()
    r = subprocess.run(args, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=1200)
    return (time.perf_counter() - s) * 1000, r


def project(d: Path, name: str, value: int) -> None:
    shutil.rmtree(d, ignore_errors=True)
    (d / "src").mkdir(parents=True)
    (d / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n')
    (d / "src/main.rs").write_text(
        f'fn add(a: u64) -> u64 {{\n    (a + {value})\n}}\nfn main() {{\n    println!("hello: {{}}", add(1));\n}}\n'
    )


def body(d: Path, value: int) -> None:
    p = d / "src/main.rs"
    p.write_text(re.sub(r"\+ \d+\)", f"+ {value})", p.read_text(), count=1))


def comment(d: Path, i: int) -> None:
    with open(d / "src/main.rs", "a") as h:
        h.write(f"// d{i}\n")


def series(label: str, args: list[str], n: int, d: Path, edit=None, warm: int = 0) -> None:
    xs: list[float] = []
    bad = 0
    for i in range(n + warm):
        want = None
        if edit:
            want = edit(i)
        ms, r = timed(args, d)
        if r.returncode != 0 or (want is not None and want not in r.stdout):
            bad += 1
        if i >= warm:
            xs.append(ms)
    xs.sort()
    q = statistics.quantiles(xs, n=4) if len(xs) >= 4 else [xs[0], xs[len(xs) // 2], xs[-1]]
    row(label, f"median {statistics.median(xs):.1f} ms (p25 {q[0]:.1f}, p75 {q[2]:.1f}, n={len(xs)}, bad={bad})")


def daemon() -> int | None:
    for p in Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            cmd = (p / "cmdline").read_bytes().split(b"\0")
            env = (p / "environ").read_bytes().split(b"\0")
        except OSError:
            continue
        if b"serve" in cmd and f"BLAZE_HOME={HOME}".encode() in env:
            return int(p.name)
    return None


def memory(label: str) -> None:
    pid = daemon()
    if pid is None:
        row(label, "no daemon found")
        return
    status = dict(l.split(":", 1) for l in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in l)
    mb = lambda k: int(status[k].split()[0]) // 1024
    rollup = dict(l.split(":", 1) for l in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:] if ":" in l)
    row(label, f"resident {mb('VmRSS')} MB (anonymous {mb('RssAnon')}, file {mb('RssFile')}, shared {mb('RssShmem')}), "
               f"huge pages {int(rollup['AnonHugePages'].split()[0]) // 1024} MB")


def explain(label: str, d: Path) -> None:
    _, r = timed([IN.blaze, "explain", "edit_work"], d)
    if r.returncode != 0:
        row(label, f"`blaze explain edit_work` failed: {r.stderr.strip()[-300:]}")
        return
    got: dict[str, int] = {}
    for line in r.stdout.splitlines():
        cells = [c.strip() for c in re.split(r"[|│]", line) if c.strip()]
        if len(cells) >= 3 and cells[2].isdigit():
            got[f"{cells[0]}.{cells[1]}"] = int(cells[2])
    keep = ("changed set.visited", "changed set.changed", "changed set.edges", "changed set.written",
            "engine memos.executed", "engine memos.verified")
    row(label, ", ".join(f"{k} {got[k]}" for k in keep if k in got) or r.stdout.strip()[-400:])


def census() -> None:
    logs = sorted(HOME.rglob("serve.log"))
    text = "".join(p.read_text(errors="replace") for p in logs)
    at = text.rfind("durability census:")
    print(text[at:at + 6000] if at >= 0 else f"no census in {logs}", flush=True)


def main() -> None:
    shutil.rmtree(WORK, ignore_errors=True)
    HOME.mkdir(parents=True)
    a = WORK / "hello-a"
    project(a, "hello-a", 1)
    row("binary", IN.blaze)
    row("load average at start", os.getloadavg()[0])
    ms, r = timed([IN.blaze, "run", "-q"], a)
    row("cold build and run", f"{ms / 1000:.2f} s, exit {r.returncode}")
    if r.returncode != 0:
        print(r.stderr[-3000:])
        return
    body(a, 100)
    ms, r = timed([IN.blaze, "run", "-q"], a)
    row("first edit after the cold build", f"{ms:.0f} ms, exit {r.returncode}, right {'hello: 101' in r.stdout}")
    explain("first edit: engine work", a)
    if IN.census:
        time.sleep(3)
        census()
        subprocess.run([IN.blaze, "stop"], cwd=a, env=ENV, capture_output=True, timeout=300)
        return

    def edit_body(i: int) -> str:
        body(a, 300 + i)
        return f"hello: {301 + i}"

    def edit_comment(i: int) -> str:
        comment(a, i)
        return "hello"

    series("no-op build", [IN.blaze, "build", "-q"], IN.n, a, warm=2)
    series("no-op run", [IN.blaze, "run", "-q"], IN.n, a, warm=2)
    series("body edit + run", [IN.blaze, "run", "-q"], IN.n, a, edit=edit_body)
    series("comment edit + run", [IN.blaze, "run", "-q"], IN.n, a, edit=edit_comment)
    time.sleep(5)
    memory("idle 5 s after the edits")
    time.sleep(65)
    memory("idle 70 s after the edits")
    b = WORK / "hello-b"
    project(b, "hello-b", 7)
    t = [timed([IN.blaze, "run", "-q"], b) for _ in range(2)]
    row("second project, warm daemon: first and second run",
        f"{t[0][0] / 1000:.2f} s, {t[1][0]:.0f} ms, bad={sum(1 for _, r in t if r.returncode != 0 or 'hello: 8' not in r.stdout)}")
    memory("idle after the second project")
    subprocess.run([IN.blaze, "stop"], cwd=a, env=ENV, capture_output=True, timeout=300)
    ms, r = timed([IN.blaze, "run", "-q"], a)
    row("first no-op run after a restart", f"{ms / 1000:.2f} s, exit {r.returncode}")
    body(a, 777)
    ms, r = timed([IN.blaze, "run", "-q"], a)
    row("first edit after a restart", f"{ms / 1000:.2f} s, exit {r.returncode}, right {'hello: 778' in r.stdout}")
    explain("first edit after a restart: engine work", a)
    time.sleep(5)
    memory("idle after the restart's builds")
    row("load average at end", os.getloadavg()[0])
    subprocess.run([IN.blaze, "stop"], cwd=a, env=ENV, capture_output=True, timeout=300)
    print(json.dumps({"label": IN.label, "rows": ROWS}))


main()
