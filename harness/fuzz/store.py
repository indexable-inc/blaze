#!/usr/bin/env python3
"""store.py: generated programs and changed asm! bodies built by v2 from an empty and a warm store.

    uv run --frozen python harness/fuzz/store.py '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/fuzz/store.py '{"blaze": "...", "profiles": ["release"]}'

`blaze` is the v2 command line; `profiles` (dev and release) the cargo profiles built, which pick
the backend (v2 compiles opt-level 0 and 1 with Cranelift, the rest with LLVM). The input is one
JSON object, inline, a file's path, or `-`. A failure keeps its work and the daemons' logs.

Issue #418: two asm! wrappers can have identical IR but different assembly, so code must be
keyed by the assembly too (2a3a0a35). One body is compiled, then only its assembly is changed,
and changed back: each version is built by a fresh daemon whose store holds what the versions
before it left (`<home>/v2/store`, with the VFS's journal and the database's image removed), and
has to run as the same source built in an empty home does. The smoke programs
(`gen.program` on fixed seeds) are built the same way: in an empty home for the reference, then
into a fresh store and again from the store that filled; every build with a store has to run as
the one from nothing does, case by case.
"""
import os, re, shutil, subprocess, sys, tempfile
from dataclasses import dataclass
from typing import Literal, get_args

try:
    import resource
except ImportError:
    resource = None
if sys.platform == "darwin":  # macOS refuses to lower RLIMIT_AS.
    resource = None

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)
from typed_input import fail, parse
import gen
from storelib import Daemon, Failed, blaze_of, forget

SCRIPT = "fuzz/store.py"
Profile = Literal["dev", "release"]
PROFILE_ARGS = {"dev": [], "release": ["--release"]}
PROGRAM = "fz"
MANIFEST = """[package]
name = "fz"
version = "0.0.0"
edition = "{edition}"

[profile.dev]
debug = 0
"""
# Seconds a generated program may run, and bytes it may map, before it is the generator's fault.
RUN_SECONDS, RUN_BYTES = 120, 8 << 30

ASM = """    #[cfg(target_arch = "x86_64")]
    #[inline(never)]
    fn transform(mut x: u64, mut y: u64) -> u64 {
        unsafe { std::arch::asm!(X86, x = inout(reg) x, y = inout(reg) y => _, options(pure, nomem, nostack)); }
        x
    }
    #[cfg(target_arch = "aarch64")]
    #[inline(never)]
    fn transform(mut x: u64, mut y: u64) -> u64 {
        unsafe { std::arch::asm!(ARM, x = inout(reg) x, y = inout(reg) y => _, options(pure, nomem, nostack)); }
        x
    }
    pub fn run() {
        for (x, y) in [(16u64, 55u64), (u64::MAX, 1), (0, 0), (0x8000_0000_0000_0000, 16)] {
            feed(transform(black_box(x), black_box(y)));
        }
    }
"""
ASSEMBLY = {
    "add": ('"add {x}, {y}"', '"add {x}, {x}, {y}"'),
    "negate": ('"neg {x}", "not {y}", "add {x}, {y}"',
               '"neg {x}, {x}", "mvn {y}, {y}", "add {x}, {x}, {y}"'),
}


@dataclass(frozen=True)
class Input:
    blaze: str
    profiles: tuple[Profile, ...] = get_args(Profile)


class Findings(Exception):
    """What was wrong, said whole."""


def edition_of(seed):
    """Half of the programs are built as 2021 and half as 2024: drop order differs between them."""
    return "2021" if sum(seed.encode()) % 2 else "2024"


def smoke_programs():
    """The programs of the short run on fixed seeds: (seed, source, edition)."""
    programs = []
    for number in range(2):
        seed = f"smoke-{number}"
        modules, _, _ = gen.program(seed, 25)
        programs.append((seed, gen.render(modules), edition_of(seed)))
    return programs


def ran(executable):
    """(status, stdout, stderr) of a program, within the limits a generated program has."""
    limit = (lambda: resource.setrlimit(resource.RLIMIT_AS, (RUN_BYTES, RUN_BYTES))) if resource else None
    try:
        done = subprocess.run([executable], capture_output=True, timeout=RUN_SECONDS, preexec_fn=limit)
    except subprocess.TimeoutExpired:
        return "timeout", "", ""
    return done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace")


def per_case(stdout):
    """The output split by case: what each printed, up to and including its checksum line."""
    cases, lines = {}, []
    for line in stdout.splitlines():
        lines.append(line)
        if re.fullmatch(r"(c\d+) [0-9a-f]{16} (ok|panic)", line):
            cases[line.split()[0]] = "\n".join(lines)
            lines = []
    return cases, "\n".join(lines)


def differences(expected, got):
    """A line per case whose output in the run `got` is not what it printed in `expected`."""
    if expected == got:
        return []
    want, _ = per_case(expected[1])
    have, rest = per_case(got[1])
    found = []
    for name, text in want.items():
        if name not in have:
            found.append(f"{name}: the program ended with status {got[0]} in it\n{rest[-600:]}\n{got[2][-600:]}")
            break
        if have[name] != text:
            found.append(f"{name}: expected\n{text}\ngot\n{have[name]}")
    return found or [f"status: expected {expected[0]} {expected[2][-300:]}\ngot {got[0]} {got[2][-300:]}"]


class Project:
    """A one-crate project at `<at>/fz`, built by daemons in the home `<at>/h`."""

    def __init__(self, blaze, at, profile):
        self.blaze, self.profile = blaze, profile
        self.dir, self.home = os.path.join(at, "fz"), os.path.join(at, "h")
        os.makedirs(os.path.join(self.dir, "src"), exist_ok=True)

    def write(self, text, edition):
        open(os.path.join(self.dir, "Cargo.toml"), "w").write(MANIFEST.format(edition=edition))
        open(os.path.join(self.dir, "src", "main.rs"), "w", encoding="utf-8").write(text)

    def build(self, what, text, edition, empty):
        """A fresh daemon builds the source, in an empty home or on the store the home has
        (and nothing else of it), and the program runs: (status, stdout, stderr)."""
        self.write(text, edition)
        if empty:
            shutil.rmtree(self.home, ignore_errors=True)
        else:
            forget(self.home)
        daemon = Daemon(self.blaze, self.home, name=re.sub(r"\W+", "-", what))
        if not daemon.listening:
            raise Failed(f"{what}: the daemon did not start:\n{daemon.log()[-2000:]}\nlog: {daemon.log_path}")
        try:
            built = daemon.build(self.dir, *PROFILE_ARGS[self.profile], program=PROGRAM)
        finally:
            daemon.stop()
        if built.status != 0 or not built.executable:
            raise Findings(f"{what} ({self.profile}): the build ended {built.status}:\n{built.said[-3000:]}\nlog: {daemon.log_path}")
        return ran(built.executable)


def held(project, what, text, edition, plain):
    got = project.build(what, text, edition, empty=False)
    found = differences(plain, got)
    if found:
        raise Findings(f"{what} ({project.profile}), from the store, runs otherwise than from nothing:\n" + "\n".join(found))


def from_nothing(project, what, text, edition):
    plain = project.build(what, text, edition, empty=True)
    if plain[0] != 0:
        raise Findings(f"{what} ({project.profile}) from nothing: status {plain[0]}\n{plain[2][-2000:]}")
    return plain


def asm_changes(blaze, work, profile):
    """One IR with two asm! bodies, each built from the store the ones before it left."""
    plain_project, stored = Project(blaze, os.path.join(work, "asm-plain"), profile), Project(blaze, os.path.join(work, "asm"), profile)
    for step, body in enumerate(("add", "negate", "add", "negate")):
        x86, arm = ASSEMBLY[body]
        text = gen.render([("c16", ASM.replace("X86", x86).replace("ARM", arm).splitlines())])
        what = f"asm {step} {body}"
        plain = from_nothing(plain_project, what, text, "2021")
        held(stored, what, text, "2021", plain)
        print(f"MATCH {what} ({profile})", flush=True)


def smoke(blaze, work, profile):
    """The smoke programs from nothing, into an empty store, and from the store that filled."""
    plain_project = Project(blaze, os.path.join(work, "smoke-plain"), profile)
    for seed, text, edition in smoke_programs():
        stored = Project(blaze, os.path.join(work, f"smoke-{seed}"), profile)
        plain = from_nothing(plain_project, seed, text, edition)
        for phase in ("cold", "warm"):
            held(stored, f"{seed} {phase}", text, edition, plain)
            print(f"MATCH {seed} {phase} ({profile})", flush=True)


def main(args):
    blaze = blaze_of(args.blaze)
    if not args.profiles:
        fail(SCRIPT, "profiles: at least one of dev, release")
    work = os.path.realpath(tempfile.mkdtemp(prefix="bz-fzst-", dir="/tmp" if sys.platform == "darwin" else None))
    print(f"work: {work}", flush=True)
    try:
        for profile in dict.fromkeys(args.profiles):
            own = os.path.join(work, profile)
            asm_changes(blaze, own, profile)
            smoke(blaze, own, profile)
    except (Failed, Findings) as failed:
        sys.exit(f"{SCRIPT}: {failed}\n  kept: {work}")
    shutil.rmtree(work)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
