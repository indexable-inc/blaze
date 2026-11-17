#!/usr/bin/env python3
"""The rows of a save built before it is asked for (lane onsave-1112, docs/v2/plan/design/on-save.md),
from one blaze binary in one fresh home. Input: one JSON value

    {"bin": "/dir/with/blaze", "work": "/dev/shm/dir", "label": "ahead", "ahead": true,
     "project": "hello", "n": 15, "warm": 30, "pauses": [0, 50, 200, 1000], "env": {"PATH": "..."}}

"project" is "hello" (no dependency) or "six" (six small crates from the registry). With
"ahead" the daemon is started with BLAZE_AHEAD=1 (set on the command that starts it alone); without, the same binary builds nothing ahead:
run both, alternating, and compare. Every command runs under one fixed environment; every run's
output and exit status are checked and the bad ones counted. Rows: the run with nothing changed,
a comment edit then a run, a body edit then a run after each pause (the run's wall time, the
pause not counted), the daemon's CPU a body edit costs from its save to a second after its run,
the daemon's memory at rest, what the daemon's log says it built ahead, and a branch switch
(300 files written at once). Prints one row a line (`label | row | value`) and, last, the rows as
one JSON line."""
import json, os, re, shutil, statistics, subprocess, sys, time
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class Input:
    bin: str
    work: str
    label: str
    ahead: bool
    project: str = "hello"
    n: int = 15
    warm: int = 30
    pauses: tuple[int, ...] = (0, 50, 200, 1000)
    env: dict[str, str] = field(default_factory=dict)


RAW = json.loads(sys.argv[1])
assert set(RAW) <= set(Input.__dataclass_fields__), RAW
IN = Input(**{**RAW, "pauses": tuple(RAW.get("pauses", (0, 50, 200, 1000)))})
assert IN.project in ("hello", "six"), IN.project
WORK = Path(IN.work).resolve()
HOME = WORK / "home"
DIR = WORK / IN.project
BLAZE = str(Path(IN.bin) / "blaze")
ENV = {"HOME": os.environ.get("HOME", "/root"), "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "BLAZE_HOME": str(HOME)}
ENV.update(IN.env)
# The daemon's own setting, on the command that starts the daemon and on no other: any `BLAZE_*`
# variable but BLAZE_HOME on a command sends the thin client through blaze-engine (6 to 8 ms).
FIRST = dict(ENV, BLAZE_AHEAD="1") if IN.ahead else ENV
ROWS: dict[str, object] = {}
BAD: list[str] = []
VALUE = [1000]

SIX = """
[dependencies]
itoa = "1"
ryu = "1"
memchr = "2"
cfg-if = "1"
bitflags = "2"
smallvec = "1"
"""
SIX_MAIN = """
bitflags::bitflags! {
    struct Flags: u8 {
        const A = 1;
    }
}
fn others() -> usize {
    let mut float = ryu::Buffer::new();
    let small: smallvec::SmallVec<[u8; 4]> = smallvec::smallvec![1, 2];
    cfg_if::cfg_if! {
        if #[cfg(unix)] { let unix = 1; } else { let unix = 0; }
    }
    float.format(1.5f64).len() + small.len() + unix + Flags::A.bits() as usize
        + memchr::memchr(b'1', b"0123").unwrap_or(0)
}
"""


def row(k: str, v: object) -> None:
    ROWS[k] = v
    print(f"{IN.label} | {k} | {v}", flush=True)


def source(value: int, comment: str = "") -> str:
    six = IN.project == "six"
    text = f"fn value() -> u64 {{\n    {value}\n}}\n"
    if six:
        text += SIX_MAIN
        text += 'fn main() {\n    let mut buffer = itoa::Buffer::new();\n    assert!(others() > 0);\n    println!("{}", buffer.format(value()));\n}\n'
    else:
        text += 'fn main() {\n    println!("{}", value());\n}\n'
    return text + comment


def write(value: int, comment: str = "") -> None:
    # In place, as an editor that truncates and writes saves.
    (DIR / "src/main.rs").write_text(source(value, comment))


def run(expect: int, what: str, env: dict[str, str] = ENV) -> float:
    began = time.perf_counter()
    done = subprocess.run([BLAZE, "run", "-q"], cwd=DIR, env=env, capture_output=True, text=True, timeout=1200)
    ms = (time.perf_counter() - began) * 1000
    if done.returncode != 0 or done.stdout.strip() != str(expect):
        BAD.append(f"{what}: exit {done.returncode}, printed {done.stdout.strip()!r} for {expect}: {done.stderr[-400:]}")
    return ms


def stats(ms: list[float]) -> str:
    ms = sorted(ms)
    return f"median {statistics.median(ms):.1f} ms (min {ms[0]:.1f}, max {ms[-1]:.1f}, n {len(ms)})"


def log() -> str:
    path = HOME / "v2/serve.log"
    return path.read_text(errors="replace") if path.exists() else ""


def pid() -> int | None:
    found = re.findall(r"\(pid (\d+)\)", log())
    return int(found[-1]) if found else None


def cpu_ms(p: int) -> float | None:
    """The daemon's user and system CPU so far."""
    try:
        if sys.platform == "linux":
            parts = Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()
            return (int(parts[11]) + int(parts[12])) * 1000 / os.sysconf("SC_CLK_TCK")
        out = subprocess.run(["ps", "-o", "cputime=", "-p", str(p)], capture_output=True, text=True).stdout.strip()
        minutes, seconds = out.rsplit(":", 1)
        hours = 0
        if ":" in minutes:
            hours, minutes = minutes.split(":")
        return (int(hours) * 3600 + int(minutes) * 60 + float(seconds)) * 1000
    except (OSError, ValueError, IndexError):
        return None


def rss_mb(p: int) -> float | None:
    try:
        if sys.platform == "linux":
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(p)], capture_output=True, text=True).stdout.strip()
        return int(out) / 1024
    except (OSError, ValueError):
        return None


def said(text: str) -> dict[str, int]:
    """What the daemon's log says it built ahead."""
    ended = re.findall(r"build ahead of \S+ (ok|failed|cancelled) in ([0-9.]+) ms: (\d+) units made", text)
    return {
        "ended ok": sum(1 for how, _, _ in ended if how == "ok"),
        "of which made a unit": sum(1 for how, _, made in ended if how == "ok" and int(made) > 0),
        "failed": sum(1 for how, _, _ in ended if how == "failed"),
        "cancelled": sum(1 for how, _, _ in ended if how == "cancelled"),
        "requests held": len(re.findall(r"waits for the build ahead under way", text)),
        "mass moves": len(re.findall(r"nothing of it is built ahead until", text)),
    }


def next_value() -> int:
    VALUE[0] += 1
    return VALUE[0]


def main() -> None:
    shutil.rmtree(WORK, ignore_errors=True)
    (DIR / "src").mkdir(parents=True)
    HOME.mkdir(parents=True)
    manifest = f'[package]\nname = "{IN.project}"\nversion = "0.1.0"\nedition = "2024"\n'
    (DIR / "Cargo.toml").write_text(manifest + (SIX if IN.project == "six" else ""))
    write(VALUE[0])
    row("load average", os.getloadavg()[0])
    row("cold run", f"{run(VALUE[0], 'cold', FIRST):.0f} ms")
    for _ in range(IN.warm):
        value = next_value()
        write(value)
        run(value, "warm")
    time.sleep(2)
    daemon = pid()
    row("daemon pid", daemon)

    row("no-op run", stats([run(VALUE[0], "no-op") for _ in range(IN.n)]))
    times = []
    for i in range(IN.n):
        write(VALUE[0], f"// comment {i}\n")
        times.append(run(VALUE[0], "comment"))
    row("comment edit + run, no pause", stats(times))
    for pause in IN.pauses:
        times, cpus = [], []
        before_log = said(log())
        for _ in range(IN.n):
            value = next_value()
            cpu0 = cpu_ms(daemon) if daemon else None
            write(value)
            time.sleep(pause / 1000)
            times.append(run(value, f"body, pause {pause}"))
            # What the daemon does after its answer (a walk ahead, bookkeeping) is the edit's.
            time.sleep(1.0)
            cpu1 = cpu_ms(daemon) if daemon else None
            if cpu0 is not None and cpu1 is not None:
                cpus.append(cpu1 - cpu0)
        after_log = said(log())
        row(f"body edit + run, pause {pause} ms", stats(times))
        if cpus:
            row(f"daemon CPU a body edit, pause {pause} ms", f"median {statistics.median(cpus):.0f} ms (min {min(cpus):.0f}, max {max(cpus):.0f})")
        row(f"built ahead, pause {pause} ms", {k: after_log[k] - before_log[k] for k in after_log})
    time.sleep(3)
    if daemon:
        row("daemon RSS at rest", f"{rss_mb(daemon):.0f} MB" if rss_mb(daemon) is not None else "not read")

    # A branch switch: 300 files at once, none a module of the program.
    before_log = said(log())
    for i in range(300):
        (DIR / "src" / f"switched_{i}.rs").write_text(f"pub fn f{i}() -> u32 {{ {i} }}\n")
    time.sleep(3)
    after_log = said(log())
    row("branch switch of 300 files, built ahead", {k: after_log[k] - before_log[k] for k in after_log})
    row("run after the switch", f"{run(VALUE[0], 'after the switch'):.0f} ms")
    time.sleep(2)
    value = next_value()
    write(value)
    time.sleep(1)
    row("body edit + run after the switch, pause 1000 ms", f"{run(value, 'edit after the switch'):.1f} ms")

    row("bad runs", len(BAD))
    for bad in BAD[:5]:
        print(f"{IN.label} | bad | {bad}", flush=True)
    row("built ahead in all", said(log()))
    subprocess.run([BLAZE, "stop"], cwd=DIR, env=ENV, capture_output=True, text=True, timeout=120)
    print(json.dumps({"label": IN.label, "rows": ROWS}, default=str))


main()
