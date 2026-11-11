#!/usr/bin/env python3
"""warm_ci.py: one commit checked and its reached tests run on a machine whose blaze daemon is
already warm (docs/v2/plan/design/ci.md, "The warm path").

usage: warm_ci.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input:

    {"checkout": "/Users/ci/blaze-warm/checkout", "remote": "https://github.com/indexable-inc/blaze",
     "commit": "<40 hex>", "init": false, "packages": [], "min_tests": 1,
     "env": [{"name": "LLVM_SYS_231_PREFIX", "value": "/usr/local/lib/blaze-llvm/<triple>"}],
     "token_var": "", "fetch_s": 120, "check_s": 1800, "test_s": 3600, "out": "warm-ci.json"}

The machine is long-lived: `checkout` is a clone that stays from job to job, and the daemon that
built it last (main, or the pull request before this one) is still up, holding its state. No VM
is made and no snapshot restored. The steps, each timed and each bounded (a step past its
`*_s` is killed and fails the run, naming the step):

  fetch   `git fetch` of `commit` into `checkout`, then a detached, forced checkout of it and a
          `git clean` of what is untracked and not ignored. The daemon's watcher sees only the
          files that differ: that is the view.
  check   `blaze check --workspace --message-format short`.
  test    `blaze test --reached --no-fail-fast --message-format json`, of `packages` (each a
          `-p`), or of harness/unittests.py's CRATES when `packages` is empty. Every test of
          those packages comes back as a `blaze-test-result` line, run or kept from a pass
          whose inputs did not move (docs/cargo.md, "Which tests run").

It fails, never passes quietly, when: `checkout` is no clone and `init` is false (`init` true
clones it, cold, and says so); a step exits nonzero or outlasts its bound; a `blaze-test-result`
line is not JSON; any test failed; or fewer than `min_tests` results came back. Under
`--reached` a test not run is still reported, so no result at all means the selection gave
nothing, which is a failure of the selection and not a pass (min_tests of 0 is refused).

`token_var`, when not empty, names an environment variable holding a token the fetch reads the
remote with: git is given it as a header for that step alone, through its environment, so it is
written to no file; the variable is taken out of the environment of every other step, so
neither blaze nor the daemon (which is sent the client's environment) ever sees it.

It writes `out` (the steps' seconds, the tests' counts, the failed tests' names), prints the
same, and adds it to $GITHUB_STEP_SUMMARY when set. The job's own status is the check run
GitHub shows: the script holds no token and posts nothing. It never stops the daemon.

Only trusted code runs here: a branch of the repository itself. A pull request from a fork
never reaches this script (the workflow's condition; ci.md, "Trust").

Needs Python 3.12 (typed_input), git, and blaze on PATH. Run from anywhere:
`PYTHONPATH=tools/typed-input/src python3 harness/warm_ci.py '{...}'`.
"""
import base64
import dataclasses
import json
import os
import re
import subprocess
import sys
import time

from typed_input import fail, parse

SCRIPT = "warm_ci.py"
HERE = os.path.dirname(os.path.abspath(__file__))
COMMIT = re.compile(r"[0-9a-f]{40}")


@dataclasses.dataclass(frozen=True)
class Var:
    name: str
    value: str


@dataclasses.dataclass(frozen=True)
class Config:
    # The clone that stays on the machine, absolute.
    checkout: str
    # What `commit` is fetched from.
    remote: str
    # The commit to check, as 40 hex digits.
    commit: str
    # Clone `checkout` when it is not there: the machine's first run, cold.
    init: bool = False
    # The packages whose tests run; empty for harness/unittests.py's CRATES.
    packages: tuple[str, ...] = ()
    # The fewest test results a run may come back with.
    min_tests: int = 1
    # Variables the steps run with besides the job's own.
    env: tuple[Var, ...] = ()
    # The environment variable holding the fetch's token; empty for a remote read without one.
    token_var: str = ""
    # Each step's bound, in seconds.
    fetch_s: int = 120
    check_s: int = 1800
    test_s: int = 3600
    # Where the result is written, as JSON.
    out: str = "warm-ci.json"


@dataclasses.dataclass
class Step:
    name: str
    seconds: float
    ok: bool
    said: str = ""


class Failed(Exception):
    """A step failed: what to say, and what the command wrote to its stdout when it was kept."""

    def __init__(self, said: str, out: str = ""):
        super().__init__(said)
        self.out = out


def run(step: str, command: list[str], cwd: str, env: dict[str, str], bound: int, capture: bool) -> str:
    """Runs `command`, its stderr (and its stdout unless `capture`) passed on; its stdout when
    `capture`. Raises `Failed` naming `step` on a nonzero exit or past `bound` seconds."""
    print(f"::group::{step}: {' '.join(command)}", flush=True)
    try:
        done = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            stdout=subprocess.PIPE if capture else None,
            text=True,
            errors="replace",
            timeout=bound,
            check=False,
        )
    except subprocess.TimeoutExpired as e:
        raise Failed(f"{step}: `{' '.join(command)}` did not end within its {bound} s and was killed") from e
    except OSError as e:
        raise Failed(f"{step}: `{' '.join(command)}` could not be run: {e}") from e
    finally:
        print("::endgroup::", flush=True)
    if done.returncode != 0:
        raise Failed(f"{step}: `{' '.join(command)}` exited {done.returncode}", done.stdout or "")
    return done.stdout or ""


def fetch(cfg: Config, env: dict[str, str]) -> None:
    git = os.path.join(cfg.checkout, ".git")
    if not os.path.exists(git):
        if not cfg.init:
            raise Failed(
                f"fetch: {cfg.checkout} is no clone, so this machine holds no warm state to check against; "
                '`"init": true` clones it once, cold'
            )
        print(f"{SCRIPT}: {cfg.checkout} is cloned now: this run is cold", flush=True)
        os.makedirs(os.path.dirname(cfg.checkout), exist_ok=True)
        run("fetch", ["git", "clone", "--no-checkout", cfg.remote, cfg.checkout], HERE, env, cfg.fetch_s, False)
    run("fetch", ["git", "fetch", "--no-tags", cfg.remote, cfg.commit], cfg.checkout, env, cfg.fetch_s, False)
    run("fetch", ["git", "checkout", "--detach", "--force", cfg.commit], cfg.checkout, env, cfg.fetch_s, False)
    # What the commit before left untracked (a file a pull request added) is not this commit's.
    run("fetch", ["git", "clean", "-fd"], cfg.checkout, env, cfg.fetch_s, False)
    head = run("fetch", ["git", "rev-parse", "HEAD"], cfg.checkout, env, cfg.fetch_s, True).strip()
    if head != cfg.commit:
        raise Failed(f"fetch: {cfg.checkout} is at {head} after checking out {cfg.commit}")


def packages(cfg: Config) -> list[str]:
    if cfg.packages:
        return list(cfg.packages)
    # The unit tests' own list, read from the commit under test: one home for it.
    sys.path.insert(0, os.path.join(cfg.checkout, "harness"))
    import unittests  # pyright: ignore[reportMissingImports]

    return list(unittests.CRATES)


def tests(cfg: Config, env: dict[str, str]) -> dict[str, object]:
    """The reached tests run: their counts and the failed ones' names. Raises `Failed`."""
    names = packages(cfg)
    command = ["blaze", "test", "--reached", "--no-fail-fast", "--message-format", "json", *(f"-p{it}" for it in names)]
    # As harness/unittests.py: blaze-diagnostics' largest test overflows a test thread's 2 MiB.
    env = {"RUST_MIN_STACK": str(16 << 20), **env}
    failed_run: Failed | None = None
    try:
        out = run("test", command, cfg.checkout, env, cfg.test_s, True)
    except Failed as e:
        # A killed run has no results to count; a failed one's are still read, to name them.
        failed_run, out = e, e.out
    total = ran = kept = ignored = outside = 0
    failed: list[str] = []
    for number, line in enumerate(out.splitlines(), 1):
        if '"blaze-test-result"' not in line:
            continue
        try:
            result = json.loads(line)
        except json.JSONDecodeError as e:
            raise Failed(f"test: line {number} of blaze's output is a blaze-test-result that is not JSON: {e}") from e
        if result.get("reason") != "blaze-test-result":
            continue
        total += 1
        outcome = result.get("outcome")
        if outcome == "failed":
            failed.append(f"{result.get('binary')}::{result.get('name')}")
        if outcome == "ignored":
            ignored += 1
        elif result.get("cached") is not None or outcome == "unreached":
            kept += 1
        else:
            ran += 1
        outside += bool(result.get("outside"))
    counts: dict[str, object] = dict(
        packages=len(names), results=total, ran=ran, kept=kept, ignored=ignored, outside_sandbox=outside, failed=sorted(failed)
    )
    if failed:
        raise FailedTests(f"test: {len(failed)} test(s) failed: {', '.join(sorted(failed)[:20])}", counts)
    if failed_run is not None:
        raise FailedTests(str(failed_run), counts)
    if total < cfg.min_tests:
        raise FailedTests(
            f"test: `blaze test --reached` came back with {total} result(s), fewer than min_tests ({cfg.min_tests}): "
            "every test of the packages is reported under --reached, run or kept, so the selection gave nothing",
            counts,
        )
    return counts


class FailedTests(Failed):
    """The test step failed with results read: their counts."""

    def __init__(self, said: str, counts: dict[str, object]):
        super().__init__(said)
        self.counts = counts


def main() -> None:
    if len(sys.argv) != 2:
        fail(SCRIPT, "usage: warm_ci.py CONFIG, one JSON object (see the top of the file)")
    cfg = parse(Config, sys.argv[1], SCRIPT)
    if not os.path.isabs(cfg.checkout):
        fail(SCRIPT, f"checkout is an absolute path, not {cfg.checkout}")
    if not COMMIT.fullmatch(cfg.commit):
        fail(SCRIPT, f"commit is 40 hex digits, not {cfg.commit}")
    if cfg.min_tests < 1:
        fail(SCRIPT, "min_tests is at least 1: a run with no test result is a selection that gave nothing")
    env = {**os.environ, **{it.name: it.value for it in cfg.env}}
    fetch_env = env
    if cfg.token_var:
        token = env.pop(cfg.token_var, "")
        if not token:
            fail(SCRIPT, f"token_var names {cfg.token_var}, which is not set or is empty")
        basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        fetch_env = {
            **env,
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "http.extraheader",
            "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {basic}",
        }
    steps: list[Step] = []
    counts: dict[str, object] = {}
    error = ""
    began = time.monotonic()
    for name in ("fetch", "check", "test"):
        at = time.monotonic()
        try:
            if name == "fetch":
                fetch(cfg, fetch_env)
                run("fetch", ["blaze", "--version"], cfg.checkout, env, cfg.fetch_s, False)
            elif name == "check":
                run("check", ["blaze", "check", "--workspace", "--message-format", "short"], cfg.checkout, env, cfg.check_s, False)
            else:
                counts = tests(cfg, env)
        except Failed as e:
            if isinstance(e, FailedTests):
                counts = e.counts
            steps.append(Step(name, time.monotonic() - at, False, str(e)))
            error = str(e)
            break
        steps.append(Step(name, time.monotonic() - at, True))
    total = time.monotonic() - began
    result = dict(
        commit=cfg.commit,
        ok=not error,
        error=error,
        seconds=round(total, 3),
        steps=[dict(name=it.name, seconds=round(it.seconds, 3), ok=it.ok, said=it.said) for it in steps],
        tests=counts,
    )
    with open(cfg.out, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=1, sort_keys=True)
        f.write("\n")
    lines = [f"### warm CI: {cfg.commit[:12]}: {'passed' if not error else 'FAILED'} in {total:.1f} s", ""]
    lines += ["| step | seconds | |", "|---|---|---|"]
    lines += [f"| {it.name} | {it.seconds:.1f} | {'ok' if it.ok else it.said} |" for it in steps]
    if counts:
        lines += ["", "tests: " + ", ".join(f"{key} {value}" for key, value in counts.items() if key != "failed")]
    text = "\n".join(lines) + "\n"
    print(text, flush=True)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    if error:
        print(f"::error::{error}")
        sys.exit(1)


if __name__ == "__main__":
    main()
