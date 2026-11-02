"""Turns a failure blaze hits building a real crate into a minimal crate.

    PYTHONPATH=python/typed-input/src python3 tools/v2/repro/repro.py '<json>'

One JSON object, its `command` one of:

- `classify`: `{"command": "classify", "log": PATH, "serve_log": PATH?}` prints the typed symptom of
  a finished build's output as JSON.
- `check`: the interestingness test. Copies `crate` into a slot, puts `candidate` in place of
  `file` (when given), runs blaze on it with `home` as `BLAZE_HOME`, and exits 0 when the symptom is
  `want`, 1 when it is not. treereduce-rust runs it with `@@.rs` as `candidate`.
- `reduce`: drops `[dependencies]`-style lines and whole files the symptom does not need, then runs
  treereduce-rust over each remaining `.rs` file, largest first, and checks the result once more on
  a fresh `BLAZE_HOME`. The reduced crate is written to `out`.

A symptom is one of `crate_key_clash`, `panic` (at a source file of blaze's), `stall`, `daemon_died`,
`error` (its first line matching a regex), `ok` (exit 0). A stall is the daemon's typed `Stall`
("blaze stalled:") when the daemon has the watchdog; until then it is no output and under a second
of the daemon's CPU for `quiet_secs`.
"""

import dataclasses
import fcntl
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "python" / "typed-input" / "src"))
from typed_input import fail, parse, read  # noqa: E402

SCRIPT = "repro.py"
ROOT = Path("/tmp/blaze-repro")
TREEREDUCE_NIX = Path(__file__).resolve().parent / "treereduce.nix"

Kind = Literal["crate_key_clash", "panic", "stall", "daemon_died", "error", "ok"]


@dataclass(frozen=True)
class Symptom:
    kind: Kind
    # `panic`: the blaze source file it panicked in, e.g. `crates/blaze-query/src/segment.rs`.
    at: str = ""
    # `error`: a regex its first `error:` line matches; for the others, extra text that must be
    # in the output (empty: none).
    text: str = ""
    # A regex the whole output must also match (empty: none), for an error whose first line
    # many failures share and whose cause is on a line after it.
    detail: str = ""


@dataclass(frozen=True)
class Classify:
    command: Literal["classify"]
    log: str
    serve_log: str = ""


@dataclass(frozen=True)
class Check:
    command: Literal["check"]
    blaze: str
    crate: str
    want: Symptom
    home: str
    file: str = ""
    candidate: str = ""
    args: tuple[str, ...] = ("build",)
    quiet_secs: int = 30
    timeout_secs: int = 900


@dataclass(frozen=True)
class Reduce:
    command: Literal["reduce"]
    blaze: str
    crate: str
    want: Symptom
    out: str
    args: tuple[str, ...] = ("build",)
    jobs: int = 4
    quiet_secs: int = 30
    timeout_secs: int = 900


@dataclass(frozen=True)
class Command:
    command: Literal["classify", "check", "reduce"]


PANIC = re.compile(r"panicked at (?:/[^\s:]*?/)?((?:crates|vendor|library)/[^\s:]+):\d+:\d+")
ERROR = re.compile(r"^error: (.*)$", re.M)


def classify(text: str, code: int | None) -> Symptom:
    """The typed symptom of a build whose output (client and daemon log) is `text`."""
    if "blaze stalled:" in text:
        return Symptom("stall")
    if "have one key" in text:
        return Symptom("crate_key_clash")
    if panic := PANIC.search(text):
        return Symptom("panic", at=panic.group(1))
    if "the daemon hung up" in text:
        return Symptom("daemon_died")
    if code == 0:
        return Symptom("ok")
    if error := ERROR.search(text):
        return Symptom("error", text=error.group(1))
    return Symptom("error", text=f"exit {code} with no error line")


def matches(got: Symptom, want: Symptom, text: str) -> bool:
    if got.kind != want.kind:
        return False
    if want.kind == "panic" and want.at and got.at != want.at:
        return False
    if want.detail and re.search(want.detail, text, re.M) is None:
        return False
    if want.kind == "error":
        return re.search(want.text, got.text) is not None
    return not want.text or re.search(want.text, text) is not None


def daemon_pid(home: Path) -> int | None:
    """The pid of the daemon `home`'s log last says is listening, if it still runs."""
    log = home / "v2" / "serve.log"
    if not log.exists():
        return None
    found = re.findall(r"listening on \S+ \(pid (\d+)\)", log.read_text(errors="replace"))
    if not found:
        return None
    pid = int(found[-1])
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return None
    return pid


def cpu_secs(pid: int | None) -> float:
    if pid is None:
        return 0.0
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return 0.0
    total = 0.0
    for part in out.replace("-", ":").split(":"):
        total = total * 60 + float(part)
    return total


def stop_daemon(home: Path) -> None:
    if (pid := daemon_pid(home)) is not None:
        os.kill(pid, signal.SIGKILL)


@dataclass(frozen=True)
class Run:
    symptom: Symptom
    text: str
    secs: float


def run_blaze(blaze: str, crate: Path, home: Path, args: tuple[str, ...], quiet_secs: int, timeout_secs: int) -> Run:
    """blaze `args` in `crate` on `home`, watched for a stall."""
    home.mkdir(parents=True, exist_ok=True)
    log = home / "v2" / "serve.log"
    serve_from = log.stat().st_size if log.exists() else 0
    env = dict(os.environ, BLAZE_HOME=str(home))
    for name in ("RUSTC", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR"):
        env.pop(name, None)
    with tempfile.TemporaryFile() as out:
        began = time.monotonic()
        proc = subprocess.Popen([blaze, *args, "--refused", "all"], cwd=crate, env=env, stdout=out, stderr=subprocess.STDOUT)
        seen, quiet_from, cpu_from, stalled = 0, began, 0.0, False
        while proc.poll() is None:
            time.sleep(1)
            now = time.monotonic()
            size = os.fstat(out.fileno()).st_size
            cpu = cpu_secs(daemon_pid(home))
            if size != seen or cpu - cpu_from >= 1.0:
                seen, quiet_from, cpu_from = size, now, cpu
            elif now - quiet_from >= quiet_secs:
                stalled = True
            if stalled or now - began >= timeout_secs:
                proc.kill()
                proc.wait()
                stop_daemon(home)
                break
        out.seek(0)
        text = out.read().decode(errors="replace")
    if log.exists():
        with log.open("rb") as f:
            f.seek(serve_from)
            text += "\n" + f.read().decode(errors="replace")
    secs = time.monotonic() - began
    if stalled and "blaze stalled:" not in text:
        text += f"\nrepro.py: no output and under 1 s of the daemon's CPU for {quiet_secs} s\n"
        return Run(Symptom("stall"), text, secs)
    if proc.returncode is not None and proc.returncode < 0 and not stalled:
        return Run(Symptom("error", text=f"timeout after {timeout_secs} s"), text, secs)
    return Run(classify(text, proc.returncode), text, secs)


def copy_crate(src: Path, dst: Path) -> None:
    if dst.exists():
        # Keep `target/`: the slot's last build is the next one's incremental start.
        for entry in dst.iterdir():
            if entry.name != "target":
                shutil.rmtree(entry) if entry.is_dir() and not entry.is_symlink() else entry.unlink()
    dst.mkdir(parents=True, exist_ok=True)
    for entry in src.iterdir():
        if entry.name == "target":
            continue
        target = dst / entry.name
        if entry.is_symlink():
            target.symlink_to(os.readlink(entry))
        elif entry.is_dir():
            shutil.copytree(entry, target, symlinks=True, ignore=shutil.ignore_patterns("target"))
        else:
            shutil.copy2(entry, target)


def check(c: Check) -> bool:
    """Whether blaze on the candidate shows `want`; one of the reduction's slots is held throughout."""
    home = Path(c.home)
    slots = home.parent / f"{home.name}-slots"
    slots.mkdir(parents=True, exist_ok=True)
    k = 0
    while True:
        lock = open(slots / f"{k}.lock", "w")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            lock.close()
            k += 1
    with lock:
        slot = slots / str(k) / Path(c.crate).name
        copy_crate(Path(c.crate), slot)
        if c.file:
            shutil.copy2(c.candidate, slot / c.file)
        run = run_blaze(c.blaze, slot, home, c.args, c.quiet_secs, c.timeout_secs)
        log = slots / f"{k}.log"
        log.write_text(run.text)
    hit = matches(run.symptom, c.want, run.text)
    print(json.dumps({"slot": k, "got": dataclasses.asdict(run.symptom), "interesting": hit, "secs": round(run.secs, 1), "log": str(log)}), file=sys.stderr)
    return hit


DEP_TABLE = re.compile(r"^\[(?:.*\.)?(?:dependencies|dev-dependencies|build-dependencies)\]\s*$")


def dependency_lines(manifest: str) -> list[int]:
    """The line numbers of single-line entries in `manifest`'s dependency tables."""
    found, inside = [], False
    for i, line in enumerate(manifest.splitlines()):
        if line.startswith("["):
            inside = DEP_TABLE.match(line) is not None
        elif inside and re.match(r"^[A-Za-z0-9_-]+\s*=", line) and line.count("{") == line.count("}"):
            found.append(i)
    return found


def reduce(r: Reduce) -> None:
    out = Path(r.out)
    if out.exists():
        fail(SCRIPT, f"{out} exists; reduce writes a new crate there")
    copy_crate(Path(r.crate), out)
    shutil.rmtree(out / "target", ignore_errors=True)
    stamp = time.strftime("%m%d%H%M%S")
    home = ROOT / f"home-{out.name}-{stamp}"
    checks = 0

    def interesting() -> bool:
        nonlocal checks
        checks += 1
        return check(Check("check", r.blaze, str(out), r.want, str(home), args=r.args, quiet_secs=r.quiet_secs, timeout_secs=r.timeout_secs))

    if not interesting():
        fail(SCRIPT, f"{r.crate} does not show {r.want} under {r.blaze}")

    # Whole dependency lines, then whole files.
    for manifest in sorted(out.rglob("Cargo.toml"), key=lambda p: -p.stat().st_size):
        for i in reversed(dependency_lines(manifest.read_text())):
            before = manifest.read_text()
            lines = before.splitlines(keepends=True)
            del lines[i]
            manifest.write_text("".join(lines))
            if not interesting():
                manifest.write_text(before)
    for path in sorted((p for p in out.rglob("*") if p.is_file() and p.name not in ("Cargo.toml", "Cargo.lock")), key=lambda p: -p.stat().st_size):
        moved = path.with_name(path.name + ".repro-dropped")
        path.rename(moved)
        if interesting():
            moved.unlink()
        else:
            moved.rename(path)

    treereduce = build_treereduce()
    for path in sorted(out.rglob("*.rs"), key=lambda p: -p.stat().st_size):
        rel = path.relative_to(out)
        reduced = path.with_name(path.name + ".reduced")
        test = [
            sys.executable, __file__,
            json.dumps({
                "command": "check", "blaze": r.blaze, "crate": str(out), "want": dataclasses.asdict(r.want),
                "home": str(home), "file": str(rel), "candidate": "@@.rs", "args": list(r.args),
                "quiet_secs": r.quiet_secs, "timeout_secs": r.timeout_secs,
            }),
        ]
        done = subprocess.run([treereduce, "-j", str(r.jobs), "-s", str(path), "-o", str(reduced), "--", *test])
        if done.returncode != 0:
            fail(SCRIPT, f"treereduce-rust on {rel} exited {done.returncode}")
        reduced.replace(path)

    # The reduction ran on one daemon kept warm across candidates; the result must hold on its own.
    stop_daemon(home)
    fresh = ROOT / f"home-{out.name}-{stamp}-confirm"
    final = Check("check", r.blaze, str(out), r.want, str(fresh), args=r.args, quiet_secs=r.quiet_secs, timeout_secs=r.timeout_secs)
    confirmed = check(final)
    stop_daemon(fresh)
    stop_daemon(home)
    print(json.dumps({"out": str(out), "checks": checks, "confirmed_on_a_fresh_home": confirmed}))
    if not confirmed:
        fail(SCRIPT, f"{out} showed {r.want} only on the reduction's warm daemon")


def build_treereduce() -> str:
    done = subprocess.run(
        ["nix", "build", "--no-link", "--print-out-paths", "-f", str(TREEREDUCE_NIX)],
        capture_output=True, text=True,
    )
    if done.returncode != 0:
        fail(SCRIPT, f"building treereduce-rust: {done.stderr.strip()}")
    return done.stdout.strip().splitlines()[-1] + "/bin/treereduce-rust"


def main() -> None:
    if len(sys.argv) != 2:
        fail(SCRIPT, "takes one JSON argument")
    value = read(sys.argv[1])
    if not isinstance(value, dict):
        fail(SCRIPT, "the input is a JSON object")
    command = parse(Command, json.dumps({"command": value.get("command")}), SCRIPT).command
    if command == "classify":
        c = parse(Classify, sys.argv[1], SCRIPT)
        text = Path(c.log).read_text(errors="replace")
        if c.serve_log:
            text += "\n" + Path(c.serve_log).read_text(errors="replace")
        code = re.findall(r"^EXIT=(\d+)", text, re.M)
        print(json.dumps(dataclasses.asdict(classify(text, int(code[-1]) if code else None))))
    elif command == "check":
        sys.exit(0 if check(parse(Check, sys.argv[1], SCRIPT)) else 1)
    else:
        reduce(parse(Reduce, sys.argv[1], SCRIPT))


if __name__ == "__main__":
    main()
