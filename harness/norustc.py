#!/usr/bin/env python3
"""norustc.py: how many times rustc and rustdoc run in each scenario of a small workspace built
with `blaze`, against the counts recorded in norustc/baseline.json. A count above its
baseline fails: blaze is to run no rustc, and a change may only lower the counts
(docs/no-rustc.md).

    norustc.py [--record] [--keep]

The workspace, norustc/fixture, has a program with an `asm!` block, a library with unit tests,
doctests and features, which the root package uses through another library, a proc macro, and a path dependency outside the workspace with a build script that
`include!`s a file, `include_str!`s another, reads a variable with `option_env!` and expands the proc macro. It
is built with a copy of the pinned toolchain whose rustc, rustdoc and cargo write a line for
each run and then become the real tool, linked with rustup under a name of its own for the run.
A scenario ends once no counted tool runs and the log has been still for a while, so what the
daemon does after a command is counted with it.

--record writes this platform's counts into the baseline (the fixture is built in the dev
profile, which v2 compiles with Cranelift), after a change that
lowered them. Where a count differs from run to run (cargo's own cache of what rustc answered
is raced by the cargos blaze runs beside it), the baseline is the highest seen. --keep leaves the work directory. Linux and macOS: the stand-ins are shell
scripts.
"""
import json, os, platform, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BLAZE = os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
FIXTURE = os.path.join(HERE, "norustc", "fixture")
BASELINE = os.path.join(HERE, "norustc", "baseline.json")
SYSTEM = {"linux": "linux", "darwin": "macos"}[sys.platform]
ARCH = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
PLACE = f"{SYSTEM}-{ARCH}"
COUNTED = ("rustc", "rustdoc", "cargo")
# How long the log has to be still, with no counted tool running, for a step to be over.
STILL = 4

# The stand-in for a tool: one line per run (the tool, its pid, its parent's command line, its
# arguments), then the real tool, which keeps the pid.
STAND_IN = """#!/bin/sh
parent=$(ps -o args= -p $PPID 2>/dev/null | cut -c1-300 | tr '\\t\\n' '  ')
printf '%s\\t%s\\t%s\\t%s\\n' {tool} $$ "$parent" "$(echo "$*" | cut -c1-1500 | tr '\\t\\n' '  ')" >> {log}
exec {real} "$@"
"""

MAIN, LIB = "src/main.rs", "word/src/lib.rs"
# The scenarios, each a list of (step, blaze arguments, edits, whether to wait for the
# daemon to be ready). An edit is (file, old, new), or (file, None, text to add).
SCENARIOS = [
    ("cold build", [("cold", ["build"], [], True)]),
    ("edits", [
        ("nothing edited", ["build"], [], False),
        ("a body", ["build"], [(MAIN, '"{} words', '"{} words!')], False),
        ("a library's body", ["build"], [(LIB, ".count()", ".filter(|it| !it.is_empty()).count()")], False),
        ("a new function", ["build"], [(MAIN, "fn main() {", "fn more() -> u32 {\n    3\n}\n\nfn main() {\n    more();")], False),
        ("an error", ["build"], [(MAIN, "    more();", '    let _wrong: u32 = "no";\n    more();')], False),
        ("the error mended", ["build"], [(MAIN, '    let _wrong: u32 = "no";\n', "")], False),
    ]),
    ("tests", [
        ("first test --no-run", ["test", "--no-run", "--workspace"], [], True),
        ("a body", ["test", "--no-run", "--workspace"], [(MAIN, '"{} words!', '"{} words!!')], False),
        ("a test added", ["test", "--no-run", "--workspace"], [(LIB, None, "\n#[test]\nfn added() {}\n")], False),
    ]),
    ("doctests", [
        ("first", ["test", "--doc", "-p", "word"], [], False),
        ("again", ["test", "--doc", "-p", "word"], [], False),
    ]),
    ("check", [("check", ["check"], [], False)]),
    # With no build of its flags loaded the daemon loads the workspace to check it.
    ("check unloaded", [
        ("the daemon stopped", ["stop"], [], False),
        ("check", ["check"], [], False),
        ("an error", ["check"], [(MAIN, "fn main() {", 'fn wrong() -> u32 {\n    "no"\n}\n\nfn main() {')], False),
        ("the error mended", ["check"], [(MAIN, 'fn wrong() -> u32 {\n    "no"\n}\n\n', "")], False),
        ("another package", ["check", "-p", "word"], [], False),
    ]),
]


def run(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, stdin=subprocess.DEVNULL, **kwargs)


def counted_toolchain(work, log):
    """A copy of the pinned toolchain whose rustc, rustdoc and cargo are stand-ins. rustc finds
    its sysroot by where its library is, so the libraries are copied and the rest linked."""
    sysroot = run(["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"], check=True).stdout.strip()
    toolchain = os.path.join(work, "toolchain")
    os.makedirs(os.path.join(toolchain, "bin"))
    os.makedirs(os.path.join(toolchain, "lib"))
    for entry in os.listdir(sysroot):
        if entry not in ("bin", "lib"):
            os.symlink(os.path.join(sysroot, entry), os.path.join(toolchain, entry))
    for entry in os.listdir(os.path.join(sysroot, "lib")):
        source, copy = os.path.join(sysroot, "lib", entry), os.path.join(toolchain, "lib", entry)
        if os.path.isdir(source):
            os.symlink(source, copy)
        else:
            shutil.copy2(source, copy)
    for entry in os.listdir(os.path.join(sysroot, "bin")):
        source, copy = os.path.join(sysroot, "bin", entry), os.path.join(toolchain, "bin", entry)
        if entry not in COUNTED:
            os.symlink(source, copy)
            continue
        shutil.copy2(source, copy + ".real")
        with open(copy, "w") as out:
            out.write(STAND_IN.format(tool=entry, log=log, real=copy + ".real"))
        os.chmod(copy, 0o755)
    return toolchain


def counts(lines):
    """The runs of each tool in lines of the log, and how many of rustc's only asked."""
    runs = {tool: 0 for tool in COUNTED}
    questions = 0
    for line in lines:
        tool, _, _, args = (line.split("\t") + ["", "", ""])[:4]
        runs[tool] = runs.get(tool, 0) + 1
        words = args.split()
        kinds = [w.split("=", 1)[1] for w in words if w.startswith("--print=")]
        kinds += [b for a, b in zip(words, words[1:]) if a == "--print"]
        printing = kinds and not any(k.startswith(("link-args", "native-static-libs")) for k in kinds)
        if tool == "rustc" and (printing or {"-vV", "-V", "--version"} & set(words)):
            questions += 1
    return runs, questions


def alive(pid):
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    state = run(["ps", "-o", "stat=", "-p", str(pid)]).stdout.strip()
    return bool(state) and "Z" not in state


def settle(log, limit=600):
    """Waits until no counted tool runs and the log has been still for STILL seconds."""
    began, size, still = time.time(), -1, 0.0
    while time.time() - began < limit:
        lines = open(log).read().splitlines()
        pids = [int(line.split("\t")[1]) for line in lines[-200:] if line.count("\t") >= 3]
        busy = any(alive(pid) for pid in pids)
        now = os.path.getsize(log)
        still = still + 0.5 if (now == size and not busy) else 0.0
        size = now
        if still >= STILL:
            return
        time.sleep(0.5)


def apply(ws, edits):
    for path, old, new in edits:
        path = os.path.join(ws, path)
        text = open(path).read()
        assert old is None or old in text, (path, old)
        open(path, "w").write(text + new if old is None else text.replace(old, new, 1))


def main():
    record, keep = "--record" in sys.argv, "--keep" in sys.argv
    # Under /tmp on macOS: the daemon's socket is in BLAZE_HOME below, and under $TMPDIR
    # (/var/folders/...) its path passes the 104 bytes macOS has for one.
    work = tempfile.mkdtemp(prefix="blaze-norustc-", dir="/tmp" if sys.platform == "darwin" else None)
    log = os.path.join(work, "runs.log")
    open(log, "w").close()
    name = f"blaze-norustc-{os.getpid()}"
    toolchain = counted_toolchain(work, log)
    run(["rustup", "toolchain", "link", name, toolchain], check=True)
    ws = os.path.join(work, "ws")
    shutil.copytree(FIXTURE, ws)
    home = os.path.join(work, "home")
    os.makedirs(home, mode=0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("BLAZE_", "RUSTC", "RUSTDOC"))}
    for unset in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO"):
        env.pop(unset, None)
    # Flags the shell gives a target (nix's x86-64 Linux shell sets
    # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS) have cargo ask rustc about the target again
    # under them: six more runs that only ask in the cold build.
    for key in [k for k in env if k.endswith("RUSTFLAGS")]:
        del env[key]
    env.update(RUSTUP_TOOLCHAIN=name, BLAZE_HOME=home,
               PATH=os.path.dirname(os.path.abspath(BLAZE)) + os.pathsep + env["PATH"],
               CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", CARGO_NET_OFFLINE="true")
    found, failed = {}, []
    try:
        for scenario, steps in SCENARIOS:
            began = len(open(log).read().splitlines())
            for step, args, edits, wait in steps:
                apply(ws, edits)
                ran = run(["blaze", *args], cwd=ws, env=env, timeout=1200)
                if wait and ran.returncode == 0:
                    for _ in range(600):
                        status = run(["blaze", "status"], cwd=ws, env=env).stdout
                        if ": ready" in status:
                            break
                        time.sleep(1)
                settle(log)
                said = (ran.stderr.strip().splitlines() or [""])[-1][:100]
                print(f"  {scenario}: {step}: exit {ran.returncode}  {said}", flush=True)
                if scenario == "doctests" and ran.returncode != 0:
                    failed.append(f"{scenario}: {step}: {said}")
            lines = open(log).read().splitlines()[began:]
            runs, questions = counts(lines)
            found[scenario] = {"rustc": runs["rustc"], "rustdoc": runs["rustdoc"]}
            with open(os.path.join(work, f"{scenario.replace(' ', '-')}.log"), "w") as out:
                out.write("\n".join(lines) + "\n")
            print(f"{scenario}: rustc {runs['rustc']} ({questions} only asking), rustdoc {runs['rustdoc']}, "
                  f"cargo {runs['cargo']}", flush=True)
    finally:
        run(["blaze", "stop"], cwd=ws, env=env)
        run(["rustup", "toolchain", "uninstall", name])
    baseline = json.load(open(BASELINE)) if os.path.exists(BASELINE) else {}
    recorded = baseline.get(PLACE)
    if record:
        baseline[PLACE] = found
        with open(BASELINE, "w") as out:
            json.dump(baseline, out, indent=1, sort_keys=True)
            out.write("\n")
        print(f"recorded the counts of {PLACE} in {BASELINE}")
    risen, fallen = [], []
    for scenario, now in found.items():
        for tool, count in now.items():
            was = (recorded or {}).get(scenario, {}).get(tool)
            if was is not None and count > was:
                risen.append(f"{scenario}: {tool} ran {count} times, {was} recorded")
            elif was is not None and count < was:
                fallen.append(f"{scenario}: {tool} ran {count} times, {was} recorded")
    if not keep:
        shutil.rmtree(work, ignore_errors=True)
    else:
        print(f"kept {work}")
    if recorded is None and not record:
        print(f"no counts recorded for {PLACE}; norustc.py --record records these:\n{json.dumps(found)}")
    for line in fallen:
        print(f"lower: {line}; norustc.py --record lowers the baseline")
    for line in risen:
        print(f"RISEN: {line}")
    for line in failed:
        print(f"FAILED: {line}")
    if risen and not record:
        print("blaze runs rustc more than it did: docs/no-rustc.md says why that is refused")
        sys.exit(1)
    if failed:
        print("the fixture's doctests failed: they pass under rustdoc")
        sys.exit(1)


if __name__ == "__main__":
    main()
