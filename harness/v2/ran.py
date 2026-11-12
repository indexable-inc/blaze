"""ran.py: a check or a measurement counts only with proof that the thing ran.

The class it closes is vacuous success (agents.json A39, A4): a timing row for a wrapper that
failed before blaze started, a `no-op run 18 ms` reported without its exit status, a
`cargo test <filter>` that matched no test and printed `test result: ok. 0 passed`, read as
`pass 6, fail 0`. Each reported the absence of a failure. Here a result exists only as the
presence of the thing:

- a timing goes through `run` (one command) or `series` (a row): the command runs under a fixed
  environment the caller gives whole (nothing inherited), its exit status is recorded and held to
  the one expected, and the caller's `Proof` that the thing ran (text it must print, a file it
  must produce, a value that must advance) is checked. A `Row` holds numbers only when every run
  had all three; otherwise it is `INVALID: <which run, why>` and holds no number at all.
- a test run goes through `tests`: its output is held to a `Tests` expectation (at least N tests
  ran, these named tests ran), and a run that ran less is invalid whatever its exit status.

A library for the harness scripts under harness/v2 (integrate.py), and a script:

    python3 -I harness/v2/ran.py selftest
    python3 -I harness/v2/ran.py '{"label": "no-op run", "command": {"args": ["blaze", "run", "-q"], "cwd": "/private/tmp/hello", "env": {"PATH": "/usr/bin:/bin", "HOME": "/Users/me"}}, "proof": {"stdout": "hello"}, "n": 30, "warm": 2}'

The script's input is one JSON object (`Input`), unknown fields refused; it prints the row and
exits 1 when the row is INVALID. `selftest` needs no blaze: it runs each rule on a shell command
and, beside it, the planted bug (the reading that looks only for a failure) that the rule
refuses.
"""
import dataclasses
import fcntl
import json
import os
import pty
import re
import struct
import subprocess
import sys
import termios
import time
import types
import typing
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Terminal:
    """A terminal the command runs on: stdin, stdout and stderr are one pty of this size."""
    cols: int = 80
    rows: int = 40


@dataclass(frozen=True)
class Command:
    """One command, and everything it runs under."""
    args: tuple[str, ...]
    cwd: str
    # The whole environment: nothing of the caller's is inherited, so two rows compare.
    env: Mapping[str, str]
    # Seconds; a command still running then is an invalid run.
    timeout: int = 900
    # The exit status a run that worked ends with.
    exit: int = 0
    # Set: the command runs on a pty of this size, and `Ran.stdout` is all it wrote there.
    terminal: Terminal | None = None


@dataclass(frozen=True)
class Proof:
    """What shows the measured thing ran, given by the caller: at least one."""
    # Text its stdout holds.
    stdout: str = ""
    # Text its stderr holds.
    stderr: str = ""
    # A file it produces: there after the run, and written since the run began.
    file: str = ""
    # A command whose stdout differs after the run from before it (a build id that advanced).
    advances: tuple[str, ...] = ()

    def stated(self) -> bool:
        return bool(self.stdout or self.stderr or self.file or self.advances)


@dataclass(frozen=True)
class Ran:
    """One run: its time, its exit status, what it wrote, and why it does not count (`invalid`,
    empty for a run that does)."""
    ms: float
    exit: int | None
    stdout: str
    stderr: str
    invalid: str = ""


def tail(text: str, most: int = 160) -> str:
    """The end of `text` on one line, for a reason."""
    return " ".join(text.strip().splitlines()[-3:])[-most:]


def spawn(command: Command) -> tuple[int | None, str, str]:
    """Runs `command` to its end: its exit status (None when it was stopped at its timeout),
    stdout and stderr."""
    env = dict(command.env)
    if command.terminal is None:
        try:
            # check=False: the exit status is the caller's to hold to the one it expects (`run`).
            done = subprocess.run(list(command.args), cwd=command.cwd, env=env, capture_output=True, text=True,
                                  errors="replace", timeout=command.timeout, stdin=subprocess.DEVNULL, check=False)
        except subprocess.TimeoutExpired as e:
            out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
            err = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
            return None, out, err
        return done.returncode, done.stdout, done.stderr
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", command.terminal.rows, command.terminal.cols, 0, 0))
    proc = subprocess.Popen(list(command.args), cwd=command.cwd, env=env, stdin=slave, stdout=slave, stderr=slave, close_fds=True)
    os.close(slave)
    out = b""
    while True:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    os.close(master)
    try:
        code: int | None = proc.wait(timeout=command.timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        code = None
    return code, out.decode(errors="replace"), ""


type Spawn = Callable[[Command], tuple[int | None, str, str]]


def run(command: Command, proof: Proof, spawn: Spawn = spawn) -> Ran:
    """One run of `command`, timed. It counts (`invalid` is empty) only when it ended with the
    exit status expected and every part of `proof` holds."""
    if not proof.stated():
        return Ran(0.0, None, "", "", "no proof was asked for: say what shows the thing ran (Proof)")
    probe = Command(proof.advances, command.cwd, command.env, command.timeout) if proof.advances else None
    before = spawn(probe) if probe else None
    began_wall = time.time()
    began = time.perf_counter()
    try:
        code, out, err = spawn(command)
    except OSError as e:
        return Ran(0.0, None, "", "", f"`{command.args[0]}` did not start: {e}")
    ms = (time.perf_counter() - began) * 1000
    why: list[str] = []
    if code is None:
        why.append(f"still running at its timeout of {command.timeout} s")
    elif code != command.exit:
        why.append(f"exit {code}, not {command.exit}: {tail(err) or tail(out) or 'it wrote nothing'}")
    if proof.stdout and proof.stdout not in out:
        why.append(f"its stdout does not hold {proof.stdout!r}: {tail(out) or 'it wrote nothing there'}")
    if proof.stderr and proof.stderr not in err:
        why.append(f"its stderr does not hold {proof.stderr!r}: {tail(err) or 'it wrote nothing there'}")
    if proof.file:
        try:
            # A second of slack: some file systems keep whole seconds.
            if os.stat(proof.file).st_mtime < began_wall - 1:
                why.append(f"{proof.file} was not written by this run: it is older than the run")
        except OSError:
            why.append(f"it did not produce {proof.file}")
    if probe and before is not None:
        after = spawn(probe)
        if after[0] != 0 or before[0] != 0:
            why.append(f"`{' '.join(proof.advances)}` failed (exit {before[0]} before, {after[0]} after)")
        elif after[1] == before[1]:
            why.append(f"`{' '.join(proof.advances)}` says {tail(after[1], 60)!r} before and after: nothing advanced")
    return Ran(ms, code, out, err, "; ".join(why))


@dataclass(frozen=True)
class Row:
    """One row of a table: `text` is what is printed. Numbers (`ms`, sorted) only when every
    run counted; else `invalid` says which did not and why, and there is no number."""
    label: str
    ms: tuple[float, ...] = ()
    invalid: str = ""
    # Runs asked for, the warm-up runs left out.
    n: int = 0

    @property
    def text(self) -> str:
        if self.invalid:
            return f"INVALID: {self.invalid}"
        xs = self.ms
        if len(xs) == 1:
            return f"{xs[0]:8.0f} ms  exit and proof checked"
        return f"median {xs[len(xs) // 2]:7.1f} ms  min {xs[0]:7.1f}  max {xs[-1]:8.1f}  n={len(xs)}, each with its exit and proof checked"

    def line(self) -> str:
        return f"{self.label:44s} {self.text}"


def series(label: str, command: Command, proof: Proof, n: int = 1, warm: int = 0,
           edit: Callable[[int], None] | None = None, proof_of: Callable[[int], Proof] | None = None,
           spawn: Spawn = spawn) -> Row:
    """`n` timed runs of `command` after `warm` untimed ones, `edit(i)` before run `i`. Every
    run, the warm-up ones too, is held to its exit status and its proof (`proof_of(i)` when the
    proof depends on the edit, else `proof`): one that fails makes the row INVALID."""
    if n < 1:
        return Row(label, invalid="no run was asked for (n < 1)")
    xs: list[float] = []
    bad: list[str] = []
    for i in range(n + warm):
        if edit:
            edit(i)
        ran = run(command, proof_of(i) if proof_of else proof, spawn)
        if ran.invalid:
            bad.append(f"run {i}: {ran.invalid}")
        elif i >= warm:
            xs.append(ran.ms)
    if bad:
        more = f" (and {len(bad) - 1} more)" if len(bad) > 1 else ""
        return Row(label, invalid=f"{len(bad)} of {n + warm} runs do not count; {bad[0]}{more}", n=n)
    return Row(label, ms=tuple(sorted(xs)), n=n)


@dataclass(frozen=True)
class Tests:
    """What a test run is expected to have run."""
    # The fewest tests that ran (passed and failed, over every result line).
    at_least: int = 1
    # Tests that ran, each by its whole name as libtest prints it.
    named: tuple[str, ...] = ()
    # Exactly this many ran, when not None: a named rerun expects its own count.
    exactly: int | None = None


@dataclass(frozen=True)
class Tested:
    """A test run, read: its counts, the tests that failed, and why it does not count
    (`invalid`, empty for a run that does). A run that counts may still have failures."""
    passed: int = 0
    failed: int = 0
    ignored: int = 0
    filtered_out: int = 0
    failures: tuple[str, ...] = ()
    invalid: str = ""

    @property
    def ran(self) -> int:
        return self.passed + self.failed

    @property
    def text(self) -> str:
        counts = f"ran {self.ran}: passed {self.passed} failed {self.failed} ignored {self.ignored} filtered out {self.filtered_out}"
        if self.invalid:
            return f"INVALID: {self.invalid} ({counts})"
        return counts + ("  " + ", ".join(self.failures)[:400] if self.failures else "")


RESULT = re.compile(r"^test result: \S+ (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out", re.MULTILINE)
COLOR = re.compile(r"\x1b\[[0-9;]*m")
TEST = re.compile(r"^test (\S+)(?: - .*)? \.\.\. (ok|FAILED|ignored)", re.MULTILINE)


def tests(stdout: str, exit: int | None, expect: Tests) -> Tested:
    """libtest's output (cargo's or blaze's, any number of test executables) held to `expect`.
    Invalid: no result line at all (it did not compile, or nothing was run), fewer tests ran than
    expected, a named test that did not run, or an exit status that disagrees with the counts
    (0 with failures, not 0 with none: it failed outside any test)."""
    text = COLOR.sub("", stdout)
    results = RESULT.findall(text)
    passed, failed, ignored, _, filtered = (sum(int(r[i]) for r in results) for i in range(5))
    said = {name: verdict for name, verdict in TEST.findall(text)}
    failures = tuple(sorted(name for name, verdict in said.items() if verdict == "FAILED"))
    ran = passed + failed
    why: list[str] = []
    if exit is None:
        why.append("it was still running at its timeout")
    if not results:
        why.append("no `test result` line: it did not compile, or ran no test executable")
    elif expect.exactly is not None and ran != expect.exactly:
        why.append(f"{ran} tests ran, not the {expect.exactly} expected ({filtered} filtered out)")
    elif ran < expect.at_least:
        why.append(f"{ran} tests ran, fewer than the {expect.at_least} expected ({filtered} filtered out, {ignored} ignored)")
    missing = [name for name in expect.named if said.get(name) not in ("ok", "FAILED")]
    if missing:
        why.append("did not run: " + ", ".join(missing))
    if results and exit is not None and (exit == 0) != (failed == 0):
        why.append(f"exit {exit} with {failed} failed tests: it failed outside any test" if failed == 0 else f"exit 0 with {failed} failed tests")
    return Tested(passed, failed, ignored, filtered, failures, "; ".join(why))


def parsed[T](cls: type[T], value: object, where: str = "the input") -> T:
    """`value` (decoded JSON) as the frozen dataclass `cls`: unknown fields, missing fields and
    values of another type are refused, naming the field (rule scripts-take-typed-input)."""
    if not isinstance(value, dict):
        # A ValueError like every other refusal of the input: `main` reports them the same way.
        raise ValueError(f"{where} is a JSON object for {cls.__name__}, not {json.dumps(value)[:80]}")  # noqa: TRY004
    given = typing.cast(dict[str, object], value)
    hints = typing.get_type_hints(cls)
    known = {f.name: f for f in dataclasses.fields(typing.cast(typing.Any, cls))}
    for key in given:
        if key not in known:
            raise ValueError(f"{where}: `{key}` is not a field of {cls.__name__} ({', '.join(sorted(known))})")
    made: dict[str, object] = {}
    for name, f in known.items():
        if name not in given:
            if f.default is dataclasses.MISSING and f.default_factory is dataclasses.MISSING:
                raise ValueError(f"{where}: {cls.__name__} needs `{name}`")
            continue
        made[name] = fitted(hints[name], given[name], f"{where}.{name}")
    return cls(**made)


def fitted(hint: object, value: object, where: str) -> object:
    """`value` as a field of type `hint`: a dataclass, a tuple of strings or of dataclasses, an
    object of strings, an int, a str or a bool, or any of those or null."""
    shown = json.dumps(value)[:80]
    origin = typing.get_origin(hint)
    if origin in (typing.Union, types.UnionType):
        options: tuple[object, ...] = typing.get_args(hint)
        if value is None and type(None) in options:
            return None
        for option in options:
            if option is not type(None):
                return fitted(option, value, where)
    if isinstance(hint, type) and dataclasses.is_dataclass(hint):
        return parsed(hint, value, where)
    if origin is tuple:
        of: object = typing.get_args(hint)[0]
        items = typing.cast(list[object], value) if isinstance(value, list) else None
        if items is not None and isinstance(of, type) and dataclasses.is_dataclass(of):
            return tuple(parsed(of, it, f"{where}[{at}]") for at, it in enumerate(items))
        if of is not str or items is None or not all(isinstance(it, str) for it in items):
            raise ValueError(f"{where} is a list of {'str' if of is str else of}, not {shown}")
        return tuple(items)
    if origin in (Mapping, dict):
        pairs = typing.cast(dict[str, object], value) if isinstance(value, dict) else None
        if pairs is None or not all(isinstance(it, str) for it in pairs.values()):
            raise ValueError(f"{where} is an object of strings, not {shown}")
        return dict(pairs)
    if hint is int or hint is str or hint is bool:
        if type(value) is not hint:
            raise ValueError(f"{where} is a {'int' if hint is int else 'str' if hint is str else 'bool'}, not {shown}")
        return value
    raise ValueError(f"{where}: ran.py reads no field of type {hint}")


@dataclass(frozen=True)
class Input:
    """The script's one JSON value: a row to time."""
    label: str
    command: Command
    proof: Proof
    n: int = 1
    warm: int = 0


def selftest() -> None:
    """Each rule on a shell command, and beside it the planted bug the rule refuses: the reading
    that looks only for a failure, which passes the same input."""
    env = {"PATH": "/usr/bin:/bin"}
    checks: list[tuple[str, bool]] = []

    def sh(script: str, timeout: int = 900, exit: int = 0, terminal: Terminal | None = None) -> Command:
        return Command(("/bin/sh", "-c", script), "/", env, timeout, exit, terminal)

    def check(name: str, held: object) -> None:
        checks.append((name, bool(held)))

    # A run that ran: a number.
    row = series("says hello", sh("echo hello"), Proof(stdout="hello"), n=3, warm=1)
    check("a run with its exit status and its proof is a number", not row.invalid and len(row.ms) == 3 and "median" in row.text)

    # A39's row: the wrapper fails before the measured thing runs.
    wrapper = sh("exit 1; echo hello")
    row = series("wrapper fails first", wrapper, Proof(stdout="hello"), n=3)
    check("a wrapper that fails before the thing ran is INVALID, with no number",
          row.text.startswith("INVALID: 3 of 3 runs do not count; run 0: exit 1, not 0") and row.ms == () and "ms" not in row.text)
    # The planted bug: time it and look for a failure phrase in what it printed.
    began = time.perf_counter()
    code, out, _ = spawn(wrapper)
    naive = f"{(time.perf_counter() - began) * 1000:.0f} ms  recomputed-note={'recomputed' in out}"
    check("the planted bug (a time and the absence of a phrase) reports that same run as a result", "ms" in naive and "False" in naive and code == 1)

    # A4's row: exit 0, and the thing did not run (the wrong program answered).
    row = series("exit 0, nothing ran", sh("true"), Proof(stdout="hello"), n=2)
    check("exit 0 without the proof is INVALID", row.invalid and "does not hold 'hello'" in row.invalid)
    check("a row with no proof asked for is INVALID", "no proof was asked for" in series("no proof", sh("true"), Proof()).invalid)
    check("a row of no runs is INVALID", series("none", sh("true"), Proof(stdout="x"), n=0).invalid)
    marker = f"/tmp/ran-selftest-{os.getpid()}"
    try:
        flaky = sh(f'if test -e {marker}; then echo hello; else touch {marker}; exit 3; fi')
        row = series("first run fails", flaky, Proof(stdout="hello"), n=3)
        check("one run that does not count makes the whole row INVALID", row.invalid.startswith("1 of 3 runs do not count; run 0: exit 3") and row.ms == ())
        # A file as proof: produced by the run, not left by an earlier one.
        os.utime(marker, (time.time() - 3600, time.time() - 3600))
        check("a file an earlier run left is no proof", "older than the run" in run(sh("true"), Proof(file=marker)).invalid)
        check("a file the run wrote is proof", not run(sh(f"echo x > {marker}"), Proof(file=marker)).invalid)
        check("a file that is not produced is no proof", "did not produce" in run(sh("true"), Proof(file=marker + ".none")).invalid)
        # A value that must advance (a build id).
        check("a value that did not advance is no proof", "nothing advanced" in run(sh("true"), Proof(advances=("/bin/cat", marker))).invalid)
        check("a value that advanced is proof", not run(sh(f"echo y >> {marker}"), Proof(advances=("/bin/cat", marker))).invalid)
    finally:
        if os.path.exists(marker):
            os.unlink(marker)
    check("a command that does not exist is INVALID", "did not start" in run(Command(("/nonexistent/blaze",), "/", env), Proof(stdout="x")).invalid)
    check("a command still running at its timeout is INVALID", "timeout" in run(sh("sleep 5", timeout=1), Proof(stdout="x")).invalid)
    check("the exit status expected may be another", not run(sh("echo no; exit 4", exit=4), Proof(stdout="no")).invalid)
    on_terminal = run(sh("test -t 1 && echo terminal", terminal=Terminal(cols=120)), Proof(stdout="terminal"))
    check("a command on a terminal is held the same way", not on_terminal.invalid)

    # Test runs. A39: the filter matched nothing, libtest says ok.
    none = "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.00s\n\n"
    read = tests(none, 0, Tests())
    check("a test run whose filter matched nothing is INVALID", read.invalid.startswith("0 tests ran, fewer than the 1 expected (212 filtered out") and read.text.startswith("INVALID"))
    # The planted bug: count `test result: ok` lines as passes, and the failures listed.
    naive_pass = len(re.findall(r"test result: ok", none)), len(re.findall(r"FAILED", none))
    check("the planted bug (count `test result: ok`) reports that run as pass 1, fail 0", naive_pass == (1, 0))
    some = ("\nrunning 2 tests\ntest revisions::a_body_edit ... ok\ntest revisions::a_rename ... FAILED\n\n"
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 40 filtered out; finished in 0.31s\n")
    read = tests(some, 101, Tests(named=("revisions::a_body_edit", "revisions::a_rename")))
    check("a run that ran what was named counts, with its failures", not read.invalid and read.failures == ("revisions::a_rename",) and (read.passed, read.failed) == (1, 1))
    check("a named test that did not run is INVALID", "did not run: revisions::gone" in tests(some, 101, Tests(named=("revisions::gone",))).invalid)
    check("a run of another count than the one expected is INVALID", "2 tests ran, not the 1 expected" in tests(some, 101, Tests(exactly=1)).invalid)
    check("no result line at all (it did not compile) is INVALID", "no `test result` line" in tests("error[E0425]: cannot find value\n", 101, Tests()).invalid)
    ok = "test a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
    check("an exit status that disagrees with the counts is INVALID", "failed outside any test" in tests(ok, 101, Tests()).invalid and not tests(ok, 0, Tests()).invalid)
    colored = ok.replace("test result: ok.", "test result: \x1b[32mok\x1b[0m.")
    check("a coloured result line is read", tests(colored, 0, Tests()).passed == 1)

    # The typed input.
    good: dict[str, object] = {"label": "x", "command": {"args": ["/bin/echo", "hi"], "cwd": "/", "env": {"PATH": "/bin"}}, "proof": {"stdout": "hi"}, "n": 2}
    made = parsed(Input, good)
    check("the input is read into frozen dataclasses", made.command.args == ("/bin/echo", "hi") and made.proof.stdout == "hi" and made.n == 2)
    refused: tuple[tuple[dict[str, object], str], ...] = (
        ({**good, "retries": 3}, "`retries` is not a field of Input"),
        ({**good, "n": "2"}, "the input.n is a int"),
        ({"label": "x", "proof": {}}, "Input needs `command`"),
        ({**good, "proof": {"stdout": "hi", "grep": "ok"}}, "`grep` is not a field of Proof"),
    )
    for bad, said in refused:
        try:
            parsed(Input, bad)
            check(f"refused: {said}", False)
        except ValueError as e:
            check(f"refused: {said}", said in str(e))

    failed = [name for name, held in checks if not held]
    for name, held in checks:
        print(f"{'ok    ' if held else 'FAILED'} {name}")
    # The self-test holds itself to its own rule: it states how many checks it expects to have run.
    expected = 29
    if len(checks) != expected:
        sys.exit(f"ran.py selftest: {len(checks)} checks ran, not the {expected} expected: a check was added or lost without this count")
    if failed:
        sys.exit(f"ran.py selftest: {len(failed)} of {len(checks)} checks failed")
    print(f"ran.py selftest: {len(checks)} of {expected} checks ran and hold")


def main(argv: list[str]) -> None:
    if len(argv) != 1:
        sys.exit("usage: ran.py selftest | ran.py '<one JSON object>' (see the top of the file)")
    if argv[0] == "selftest":
        return selftest()
    text = sys.stdin.read() if argv[0] == "-" else Path(argv[0]).read_text() if os.path.isfile(argv[0]) else argv[0]
    try:
        asked = parsed(Input, json.loads(text))
    except (json.JSONDecodeError, ValueError) as e:
        sys.exit(f"ran.py: {e}")
    row = series(asked.label, asked.command, asked.proof, asked.n, asked.warm)
    print(row.line(), flush=True)
    sys.exit(1 if row.invalid else 0)


if __name__ == "__main__":
    main(sys.argv[1:])
