"""downstream.py: ruff, ty and uv's own test suites under blaze, at pinned commits.

    downstream.py setup '{"suite": "uv", "checkout": "downstream", "scratch": "/tmp/s"}'
        what the suite needs of the runner: user namespaces for the sandbox, swap, a target
        directory on the largest disk, and for uv's suites the Pythons, the git, keyring and
        filesystems uv's tests use (a tmpfs, ext4's hardlink limit). Writes what later steps
        need to GITHUB_ENV.
    downstream.py test '{"suite": "uv", "checkout": "downstream", "scratch": "/tmp/s"}'
        the suite under `blaze test` (downstream.yaml's choices):
          ruff-and-ty          ruff's workspace, ty included, every feature
          uv                   uv's workspace with its Linux test features; the three cases
                               sharing one inline snapshot each run in a process of its own,
                               and the copy-on-write tests' executables packed for `cow`
          uv-archive-fixtures  uv's 39 ZIP extraction cases alone
          uv-lock-edit         an edit of a uv lock diagnostic and its test's assertion
          ty-semantic-edit     an edit of a ty diagnostic and its mdtest's assertion
        Every run has to say blaze built it (cargo's `Finished` line with blaze's tag). An edit
        suite runs its witness three times: unchanged (passes), with the diagnostic and its
        assertion edited (passes), and with the diagnostic restored under the edited
        assertion (fails, printing the edited text): an executable the edit did not reach
        passes that last run.
    downstream.py cow '{"checkout": "downstream", "scratch": "/tmp/s", "archive": "/tmp/s/uv-cow.tar.gz"}'
        uv's five copy-on-write tests on a Btrfs loop image, each in a process of its own, with
        the executables `test` built: the build runner has no loop devices.

The JSON is inline, a file's path, or `-` for stdin. Linux, with passwordless sudo and the uv of
downstream.yaml on PATH. Python 3.13, from the workspace at the repository's root:
`uv run --frozen --project <blaze> python <blaze>/.github/scripts/downstream.py ...`.

Ported from astral-experiments/blaze#38, #41, #44 and #80 (Charlie Marsh).
"""

import asyncio
import os
import re
import shutil
import sys
import tempfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import fail, parse

SCRIPT = "downstream.py"
type Suite = Literal["ruff-and-ty", "uv", "uv-archive-fixtures", "uv-lock-edit", "ty-semantic-edit"]

# What a run of blaze's prints when blaze built it: cargo's `Finished` line, tagged with blaze's
# name (`blaze`, or `blaze, remote down: ...`). blaze hands nothing to cargo (docs/no-rustc.md),
# so a run without it built nothing, whatever its status.
# Colour, which a test's own output may have, out of what is matched.
ANSI = re.compile(r"\x1b\[[0-9;]*m")
BUILT_BY_BLAZE = re.compile(r"`[^`]+` profile \[blaze[^\]]*\] target\(s\) in ")
# The environment every suite's tests run in. One libtest thread: ruff's language-server tests
# install a process-wide panic hook, and uv's native authentication tests share the system's
# credential store, which upstream's nextest gives a process each.
TEST_ENV = {
    "BLAZE_TESTS": "all",
    "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
    "RUST_MIN_STACK": "33554432",
    "RUST_TEST_THREADS": "1",
    "INSTA_UPDATE": "no",
    "UV_HTTP_RETRIES": "5",
}
UV_FEATURES = "test-python-patch,test-universal,native-auth,secret-service"
# Three parameterized tests of uv-requirements-txt share one inline snapshot, which insta
# refuses a second and third assertion of in one process: each runs in one of its own.
SHARED_SNAPSHOT = "test::malformed_option::"
# uv's copy-on-write tests, by the test executable they are in; `cow` runs them on Btrfs.
COW_TESTS = {
    "build": [
        "cache_clean::clean_all_cloned_file",
        "cache_clean::clean_all_cached_clones",
        "cache_clean::clean_all_compressed_file",
    ],
    "pip_install": ["pip_install::install_executable_clone", "pip_install::install_copy_on_write_fs"],
}
# The managed Pythons uv's tests ask for besides the default ones of its checkout.
UV_PYTHONS = ["3.12d", "3.13d", "3.13t", "3.13td"]
UV_APT = [
    "git/noble-updates",
    "git-man/noble-updates",
    "libcurl3t64-gnutls/noble-updates",
    "git-lfs",
    "gnome-keyring",
]


@dataclass(frozen=True)
class Setup:
    suite: Suite
    checkout: str
    scratch: str


@dataclass(frozen=True)
class Test:
    suite: Suite
    checkout: str
    scratch: str


@dataclass(frozen=True)
class Cow:
    checkout: str
    scratch: str
    archive: str


class Failed(Exception):
    pass


@dataclass(frozen=True)
class Ran:
    status: int
    output: str


async def run(
    step: str,
    argv: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    stdin: bytes | None = None,
    minutes: float = 30,
    check: bool = True,
    daemon: bool = False,
) -> Ran:
    """Runs `argv`, its output shown as it comes and kept; with `check`, a status other than 0
    fails naming the step, the command, the status and the end of what it printed. A `daemon`
    leaves a process behind that keeps what it was given as its output: that goes to a file,
    read once the command has exited, rather than a pipe that would not end."""
    print(f"::group::{step}: {' '.join(argv)}", flush=True)
    with tempfile.TemporaryFile() as kept:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            cwd=cwd,
            env=dict(env) if env is not None else None,
            stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
            stdout=kept if daemon else asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        lines: list[str] = []
        try:
            async with asyncio.timeout(minutes * 60):
                if stdin is not None and proc.stdin is not None:
                    proc.stdin.write(stdin)
                    await proc.stdin.drain()
                    proc.stdin.close()
                if proc.stdout is not None:
                    async for raw in proc.stdout:
                        line = raw.decode(errors="replace")
                        lines.append(line)
                        sys.stdout.write(line)
                status = await proc.wait()
        except TimeoutError:
            proc.kill()
            await proc.wait()
            raise Failed(f"{step}: `{' '.join(argv)}` ran past {minutes} minutes") from None
        finally:
            if daemon:
                kept.seek(0)
                lines = [kept.read().decode(errors="replace")]
                sys.stdout.write(lines[0])
            print("::endgroup::", flush=True)
    output = ANSI.sub("", "".join(lines))
    if check and status != 0:
        raise Failed(f"{step}: `{' '.join(argv)}` exited {status}:\n{output[-3000:]}")
    return Ran(status, output)


def child_env(extra: Mapping[str, str] = {}) -> dict[str, str]:
    """This environment for a downstream command: without the virtual environment `uv run` gave
    this script, which is blaze's workspace's and not the checkout's."""
    env = {k: v for k, v in os.environ.items() if k != "VIRTUAL_ENV"}
    return {**env, **extra}


def export(name: str, value: str) -> None:
    """`name` for this step's commands and, through GITHUB_ENV, the job's later steps."""
    os.environ[name] = value
    if path := os.environ.get("GITHUB_ENV"):
        with open(path, "a") as f:
            f.write(f"{name}={value}\n")


def built_by_blaze(output: str, what: str) -> None:
    if not BUILT_BY_BLAZE.search(output):
        raise Failed(f"{what}: blaze did not say it built the tests (no `profile [blaze] target(s)` line)")


def target_dir(checkout: Path) -> Path:
    return Path(os.environ.get("CARGO_TARGET_DIR") or checkout / "target")


# ---------------------------------------------------------------------------------------------
# setup


async def setup(it: Setup) -> None:
    checkout, scratch = Path(it.checkout).resolve(), Path(it.scratch)
    scratch.mkdir(parents=True, exist_ok=True)
    # The edit of uv's lock runs its test with the default Pythons alone.
    uv = it.suite in ("uv", "uv-lock-edit")
    async with asyncio.TaskGroup() as group:
        group.create_task(user_namespaces())
        group.create_task(swap_and_target())
        if uv:
            group.create_task(uv_pythons(it.suite, checkout, scratch))
        if it.suite == "uv":
            group.create_task(uv_services(scratch))


async def user_namespaces() -> None:
    """Every build script and proc macro runs in blaze's sandbox, made of namespaces a user
    makes, which Ubuntu's AppArmor keeps users from making unless told otherwise."""
    flag = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    if flag.exists() and flag.read_text().strip() != "0":
        await run("user namespaces", ["sudo", "sysctl", "kernel.apparmor_restrict_unprivileged_userns=0"])


async def swap_and_target() -> None:
    """20 GB of swap and the target directory on the disk with the most room (/mnt on x86-64):
    ruff's and uv's test builds outgrow the root disk and a hosted runner's 16 GB."""
    disks = [Path(d) for d in ("/", "/mnt") if Path(d).is_dir()]
    disk = max(disks, key=lambda d: shutil.disk_usage(d).free)
    swap = disk / "downstream.swap"
    await run("swap", ["sudo", "fallocate", "-l", "20G", str(swap)])
    await run("swap", ["sudo", "chmod", "600", str(swap)])
    await run("swap", ["sudo", "mkswap", str(swap)])
    await run("swap", ["sudo", "swapon", str(swap)])
    target = disk / "downstream-target"
    await run("target directory", ["sudo", "mkdir", "-p", str(target)])
    await run("target directory", ["sudo", "chown", f"{os.getuid()}:{os.getgid()}", str(target)])
    export("CARGO_TARGET_DIR", str(target))


async def uv_pythons(suite: Suite, checkout: Path, scratch: Path) -> None:
    """The managed Pythons uv's tests use, from the download list of the checkout."""
    export("UV_PYTHON_CACHE_DIR", str(scratch / "uv-python-cache"))
    env = child_env({"UV_PYTHON_DOWNLOADS_JSON_URL": "crates/uv-python-managed/download-metadata.json"})
    await run("uv's Pythons", ["uv", "python", "install"], cwd=checkout, env=env, minutes=20)
    if suite == "uv":
        await run("uv's Pythons", ["uv", "python", "install", *UV_PYTHONS], cwd=checkout, env=env, minutes=20)


async def uv_services(scratch: Path) -> None:
    """git, git-lfs, a Secret Service keyring on a session bus of its own, and a tmpfs; ext4's
    hardlink limit is that of the runner's own disk."""
    await run("apt", ["sudo", "apt-get", "update", "-qq"], minutes=10)
    await run(
        "apt",
        ["sudo", "apt-get", "install", "-y", "--no-install-recommends", "--allow-downgrades", *UV_APT],
        minutes=10,
    )
    bus = f"unix:path={scratch / 'uv-test-bus'}"
    await run("session bus", ["dbus-daemon", "--session", "--fork", f"--address={bus}"], daemon=True)
    export("DBUS_SESSION_BUS_ADDRESS", bus)
    keyring = ["gnome-keyring-daemon", "--components=secrets", "--daemonize", "--unlock"]
    await run("keyring", keyring, stdin=b"uv-test", daemon=True)
    await run(
        "keyring",
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            "org.freedesktop.secrets",
            "--object-path",
            "/org/freedesktop/secrets",
            "--method",
            "org.freedesktop.Secret.Service.ReadAlias",
            "default",
        ],
    )
    await run("tmpfs", ["sudo", "mkdir", "-p", "/tmpfs"])
    await run("tmpfs", ["sudo", "mount", "-t", "tmpfs", "-o", "size=256m", "tmpfs", "/tmpfs"])
    await run("tmpfs", ["sudo", "chown", f"{os.getuid()}:{os.getgid()}", "/tmpfs"])


# ---------------------------------------------------------------------------------------------
# test


async def test(it: Test) -> None:
    checkout, scratch = Path(it.checkout).resolve(), Path(it.scratch).resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    match it.suite:
        case "ruff-and-ty":
            argv = [
                "uv",
                "run",
                "--locked",
                "--only-dev",
                "blaze",
                "test",
                "--workspace",
                "--all-features",
                "--locked",
                "--no-fail-fast",
            ]
            ran = await run("ruff and ty", argv, cwd=checkout, env=child_env(TEST_ENV), minutes=270)
            built_by_blaze(ran.output, "ruff and ty")
        case "uv":
            await uv_suite(checkout, scratch)
        case "uv-archive-fixtures":
            argv = ["blaze", "test", "--locked", "-p", "uv", "--test", "build", "--", "extract::"]
            ran = await run("uv's archive fixtures", argv, cwd=checkout, env=child_env(TEST_ENV), minutes=120)
            built_by_blaze(ran.output, "uv's archive fixtures")
            if "39 passed; 0 failed" not in ran.output:
                raise Failed("uv's archive fixtures: not all 39 extraction cases passed")
        case "uv-lock-edit" | "ty-semantic-edit":
            await edit(EDITS[it.suite], checkout)


async def uv_suite(checkout: Path, scratch: Path) -> None:
    """uv's workspace, then the shared snapshot's cases one process each, then the executables
    of the copy-on-write tests packed for `cow`. The later two run after a failing suite too,
    so long as blaze built it."""
    env = child_env(
        {
            **TEST_ENV,
            "UV_INTERNAL__TEST_NOCOW_FS": "/tmpfs",
            "UV_INTERNAL__TEST_ALT_FS": "/tmpfs",
            "UV_INTERNAL__TEST_LOWLINKS_FS": str(scratch),
        }
    )
    argv = [
        "uv",
        "run",
        "--locked",
        "--only-group",
        "test",
        "blaze",
        "test",
        "--workspace",
        "--locked",
        "--no-fail-fast",
        "--features",
        UV_FEATURES,
        "--",
        "--skip",
        SHARED_SNAPSHOT,
    ]
    suite = await run("uv", argv, cwd=checkout, env=env, minutes=240, check=False)
    built_by_blaze(suite.output, "uv")
    errors: list[str] = []
    if suite.status != 0:
        errors.append(f"uv: `{' '.join(argv)}` exited {suite.status}")
    for later in (shared_snapshot(checkout, suite.output), pack_cow(checkout, scratch, suite.output)):
        try:
            await later
        except Failed as e:
            errors.append(str(e))
    if errors:
        raise Failed("\n".join(errors))


def executables(output: str, checkout: Path) -> list[tuple[str, Path]]:
    """(target, executable) of each `Running` line of a `blaze test`: `tests/build/main.rs` or
    `unittests src/lib.rs`, and the path in parentheses, from the checkout."""
    found: list[tuple[str, Path]] = []
    for line in output.splitlines():
        words = line.split()
        if (
            len(words) >= 3
            and words[0] == "Running"
            and words[-1].startswith("(")
            and words[-1].endswith(")")
        ):
            found.append((" ".join(words[1:-1]), checkout / words[-1][1:-1]))
    return found


async def shared_snapshot(checkout: Path, output: str) -> None:
    exes = [
        exe
        for target, exe in executables(output, checkout)
        if target == "unittests src/lib.rs" and exe.name.startswith("uv_requirements_txt-")
    ]
    if len(exes) != 1:
        raise Failed(f"shared snapshot: {len(exes)} unit-test executables of uv-requirements-txt ran, not 1")
    exe = exes[0].resolve()
    package = checkout / "crates/uv-requirements-txt"
    env = child_env({**TEST_ENV, "CARGO_MANIFEST_DIR": str(package)})
    listed = await run("shared snapshot", [str(exe), "--list", SHARED_SNAPSHOT], cwd=package, env=env)
    cases = [line.removesuffix(": test") for line in listed.output.splitlines() if line.endswith(": test")]
    if len(cases) != 3:
        raise Failed(f"shared snapshot: {SHARED_SNAPSHOT} has {len(cases)} cases, not 3: {cases}")
    for case in cases:
        ran = await run(f"shared snapshot {case}", [str(exe), "--exact", case], cwd=package, env=env)
        if "1 passed; 0 failed" not in ran.output:
            raise Failed(f"shared snapshot: {case} did not pass alone")


async def pack_cow(checkout: Path, scratch: Path, output: str) -> None:
    """The test executables of COW_TESTS and uv, without their debug info, in one archive."""
    packed = scratch / "uv-cow"
    packed.mkdir(exist_ok=True)
    ran = executables(output, checkout)
    for name in COW_TESTS:
        exes = [exe for target, exe in ran if target == f"tests/{name}/main.rs"]
        if len(exes) != 1:
            raise Failed(f"copy-on-write tests: {len(exes)} executables of tests/{name}/main.rs ran, not 1")
        shutil.copy2(exes[0], packed / name)
    uv = target_dir(checkout) / "debug/uv"
    if not uv.is_file():
        raise Failed(f"copy-on-write tests: no uv at {uv}, where the tests' CARGO_BIN_EXE_uv is")
    shutil.copy2(uv, packed / "uv")
    names = [str(packed / it) for it in [*COW_TESTS, "uv"]]
    await run("copy-on-write tests", ["strip", "--strip-debug", *names])
    await run("copy-on-write tests", ["tar", "-czf", str(scratch / "uv-cow.tar.gz"), "-C", str(packed), "."])


@dataclass(frozen=True)
class Edit:
    package: str
    target: str
    group: tuple[str, ...]
    features: tuple[str, ...]
    witness: str
    source: str
    test: str
    before: str
    after: str
    old_assertion: str
    new_assertion: str


EDITS: dict[str, Edit] = {
    "uv-lock-edit": Edit(
        package="uv",
        target="lock",
        group=("--only-group", "test"),
        features=("--features", "test-python-patch,test-universal,native-auth"),
        witness="lock::lock_validation_warning_chain",
        source="crates/uv-lock-operations/src/lock.rs",
        test="crates/uv/tests/lock/lock.rs",
        before="Failed to validate existing lockfile",
        after="Could not validate the existing uv.lock",
        old_assertion="Failed to validate existing lockfile",
        new_assertion="Could not validate the existing uv.lock",
    ),
    "ty-semantic-edit": Edit(
        package="ty_python_semantic",
        target="mdtest",
        group=("--only-dev",),
        features=("--all-features",),
        witness="mdtest::subscript/stepsize_zero.md",
        source="crates/ty_python_semantic/src/types/diagnostic.rs",
        test="crates/ty_python_semantic/resources/mdtest/subscript/stepsize_zero.md",
        before="Slice step size cannot be zero",
        after="A slice step must be nonzero",
        old_assertion="values[1:10:0]  # error: [zero-stepsize-in-slice]",
        new_assertion='values[1:10:0]  # error: [zero-stepsize-in-slice] "A slice step must be nonzero"',
    ),
}


def replace_once(path: Path, text: str, before: str, after: str) -> str:
    if text.count(before) != 1:
        raise Failed(f"{path}: {before!r} is there {text.count(before)} times at the pinned commit, not once")
    return text.replace(before, after)


async def edit(it: Edit, checkout: Path) -> None:
    source, test = checkout / it.source, checkout / it.test
    original_source, original_test = source.read_text(), test.read_text()
    edited_source = replace_once(source, original_source, it.before, it.after)
    edited_test = replace_once(test, original_test, it.old_assertion, it.new_assertion)
    argv = [
        "uv",
        "run",
        "--locked",
        *it.group,
        "blaze",
        "test",
        "--locked",
        "-p",
        it.package,
        "--test",
        it.target,
        *it.features,
        "--",
        "--exact",
        it.witness,
    ]
    env = child_env(TEST_ENV)

    async def witness(step: str, passes: bool) -> None:
        ran = await run(step, argv, cwd=checkout, env=env, minutes=120, check=False)
        built_by_blaze(ran.output, step)
        summary = "ok. 1 passed; 0 failed" if passes else "FAILED. 0 passed; 1 failed"
        wrong = [
            why
            for why, ok in [
                (f"exited {ran.status}", (ran.status == 0) == passes),
                (f"no line `test {it.witness} ...`", f"test {it.witness} ..." in ran.output),
                (
                    f"no `test result: {summary}; 0 ignored`",
                    f"test result: {summary}; 0 ignored" in ran.output,
                ),
                (f"no {it.after!r} in what it printed", passes or it.after in ran.output),
            ]
            if not ok
        ]
        if wrong:
            raise Failed(
                f"{step}: {it.witness} {'passing' if passes else 'failing'} was expected: {'; '.join(wrong)}"
            )

    await witness("unchanged", True)
    try:
        source.write_text(edited_source)
        test.write_text(edited_test)
        await witness("diagnostic and assertion edited", True)
        # The new assertion stays: an executable that took neither edit would pass it.
        source.write_text(original_source)
        await witness("diagnostic restored, assertion edited", False)
    finally:
        source.write_text(original_source)
        test.write_text(original_test)


# ---------------------------------------------------------------------------------------------
# cow


async def cow(it: Cow) -> None:
    checkout, scratch = Path(it.checkout).resolve(), Path(it.scratch).resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    packed = scratch / "uv-cow"
    packed.mkdir(exist_ok=True)

    async def btrfs() -> None:
        image = scratch / "cow.img"
        await run("apt", ["sudo", "apt-get", "update", "-qq"], minutes=10)
        await run(
            "apt", ["sudo", "apt-get", "install", "-y", "--no-install-recommends", "btrfs-progs"], minutes=10
        )
        await run("btrfs", ["truncate", "-s", "1G", str(image)])
        await run("btrfs", ["mkfs.btrfs", str(image)])
        await run("btrfs", ["sudo", "mkdir", "-p", "/btrfs"])
        await run("btrfs", ["sudo", "mount", "-o", "loop", str(image), "/btrfs"])
        await run("btrfs", ["sudo", "chown", f"{os.getuid()}:{os.getgid()}", "/btrfs"])

    async def python() -> None:
        env = child_env({"UV_PYTHON_DOWNLOADS_JSON_URL": "crates/uv-python-managed/download-metadata.json"})
        await run("uv's Python", ["uv", "python", "install", "3.12"], cwd=checkout, env=env, minutes=20)

    async with asyncio.TaskGroup() as group:
        group.create_task(btrfs())
        group.create_task(python())
        group.create_task(run("unpack", ["tar", "-xzf", it.archive, "-C", str(packed)]))
    package = checkout / "crates/uv"
    env = child_env(
        {
            "INSTA_UPDATE": "no",
            "UV_INTERNAL__TEST_COW_FS": "/btrfs",
            "UV_PYTHON_DOWNLOADS_JSON_URL": "crates/uv-python-managed/download-metadata.json",
            "CARGO_BIN_EXE_uv": str(packed / "uv"),
            "CARGO_MANIFEST_DIR": str(package),
        }
    )
    failed: list[str] = []
    for exe, cases in COW_TESTS.items():
        for case in cases:
            ran = await run(
                f"copy-on-write {case}",
                [str(packed / exe), "--exact", case],
                cwd=package,
                env=env,
                check=False,
            )
            if ran.status != 0 or "1 passed; 0 failed" not in ran.output:
                failed.append(f"{case} (exit {ran.status})")
    if failed:
        raise Failed(f"copy-on-write tests on Btrfs that did not pass: {', '.join(failed)}")


def main() -> None:
    match sys.argv[1:]:
        case ["setup", text]:
            command = setup(parse(Setup, text, SCRIPT))
        case ["test", text]:
            command = test(parse(Test, text, SCRIPT))
        case ["cow", text]:
            command = cow(parse(Cow, text, SCRIPT))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} setup|test|cow JSON (see the top of the file)")
    try:
        asyncio.run(command)
    except* Failed as group:
        fail(SCRIPT, "\n".join(str(e) for e in group.exceptions))


if __name__ == "__main__":
    main()
