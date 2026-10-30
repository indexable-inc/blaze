# pyright: basic
"""ci.py: the harnesses CI runs, in parts that take about the same time on a hosted runner.

usage: ci.py run '{"part": "core-1"}'   run the harnesses of a part for BLAZE_BACKEND on this machine
       ci.py run '{"part": "fast"}'     run every harness of the fast tier here, one after another
       ci.py parts '{"tier": "full"}'   CI's matrix for this machine, as JSON: {"include": [{"backend",
                                        "part", "apt"}, ...]}, tier `full` (the default) or `fast`
       ci.py list '{}'                  which harness runs in which part, on which hosts and backends

After the subcommand comes one JSON object (inline, the path of a file, or `-` for standard
input) with the fields of its class below (Run, Parts, List); a field it does not have is
refused. Run it in the repository's Python workspace, `uv run --frozen python harness/ci.py ...`
from the root: the harnesses run in the Python that runs it, and some read their input with the
workspace's typed_input. BLAZE_BACKEND is the environment's, as the harnesses and blaze read it.

The fast tier, of every pull request and merge-queue entry, is the harnesses that FAST names,
each on the hosts FAST gives it: on Linux x86-64 all of them, and on Linux AArch64 and macOS the
ones where the platform matters (code generation and the ABI, the object format and the linker,
the sandbox, live edits, the store's files). They are packed into parts `quick-1` ... `quick-N`
of about PART_SECS each, per host and backend, by the seconds FAST says each takes on the
slowest host, so a new or slower harness moves the others, not the time of the tier. The full
tier, the release train's (every push to main) and by hand, is every harness, in the parts
core-1 ... core-N (packed as the fast tier is, to about CORE_SECS by the seconds CORE says),
keys-N, buildmodel, store-kills, store and daemon (docs/harness.md, "CI").

Each harness runs with an BLAZE_HOME of its own (a harness may set its own over it), so no
daemon or socket of one harness is seen by the next. BLAZE_CACHE is left alone: set, it turns
the store on.

Every harness in a part runs even when one before it fails; the part fails at the end and names
them. BLAZE_BIN is the blaze the harnesses run, as for each harness on its own.
"""
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from typing import Literal

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SYSTEM = {"linux": "linux", "darwin": "macos", "win32": "windows"}[sys.platform]
ARCH = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
BACKEND = os.environ.get("BLAZE_BACKEND") or "cranelift"
BLAZE_CLI = os.path.join(
    os.path.dirname(os.path.abspath(os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine"))),
    "blaze.exe" if SYSTEM == "windows" else "blaze",
)
KEYS = 3


def here(hosts):
    """Whether this machine is one of `hosts` ("linux", "macos", "windows", "linux-aarch64"...);
    no hosts is every one."""
    return not hosts or SYSTEM in hosts or f"{SYSTEM}-{ARCH}" in hosts


def arg(value):
    """A harness's one typed input, as short as JSON writes it: it is the harness's label too."""
    return json.dumps(value, separators=(",", ":"))


def on(*hosts, backend=None):
    """Whether a harness runs here under a backend (BLAZE_BACKEND's when none is given)."""
    return lambda chosen=None: here(hosts) and backend in (None, chosen or BACKEND)


EVERYWHERE = on()
# (part in the full tier, arguments, environment, where it runs, in the fast tier: None or not).
# The order is the order of each part's run. A harness in the fast tier needs a line in FAST.
HARNESSES = [
    ("core", ["difftest.py", "corpus", "--bin", "runner", "--crate", "cases", "--layouts"], {}, EVERYWHERE, 2),
    ("core", ["programs.py"], {}, EVERYWHERE, 1),
    # The daemon compiles every function of both programs, main too, and LLVM is optimizing: an
    # edit of what both use has to reach each program, whatever its callers had built into them.
    ("core", ["programs.py"], {"BLAZE_ALONE": "1"}, on("linux", "macos", backend="llvm"), 1),
    ("core", ["breakers.py"], {}, EVERYWHERE, 1),
    ("core", ["unwinds.py"], {}, EVERYWHERE, 1),
    ("core", ["growth.py"], {}, on(backend="cranelift"), 1),
    # 11 to 22 minutes a run, longer than the rest of the fast tier, and it caught nothing there
    # in 2026-10-25 and 26's 270 runs: the full tier's alone.
    ("core", ["wholeprogram.py"], {}, on("linux", "macos"), None),
    # `blaze build` from nothing: G's whole build, no cargo build.
    ("core", ["coldbuild.py"], {}, on("linux"), 2),
    # A load refused for configuration must be retried without stopping the daemon (#597).
    ("core", ["loadretry.py"], {}, on("linux", "macos"), 2),
    # A build script reads the command's environment, not the daemon's (#720).
    ("core", ["scriptenv.py"], {}, on("linux", "macos"), 2),
    # Its program of writable statics of its own crates on macOS too, where anim's `HOME` was refused.
    ("core", ["coldbuild.py", "statics"], {}, on("macos"), None),
    # std's own writable statics (`thread_local!`s, `MIN`), from a unit built on another machine (#840).
    ("core", ["stdstatics.py"], {}, on("linux", "macos"), 2),
    ("core", ["splices.py"], {}, EVERYWHERE, 2),
    ("core", ["live_e2e.py"], {}, on("linux-x86_64"), 1),
    # A type added beside another, the code moved to it and the old one deleted, each a patch.
    ("core", ["live_types.py"], {}, on("linux-x86_64"), 1),
    # A program on a nightly feature (allocator_api) builds and takes live edits the same way.
    ("core", ["live_e2e.py"], {"LIVE_PROGRAM": "live-alloc"}, on("linux-x86_64"), 1),
    # Apple's linker leaves no more room than it is told, and gives a newer macOS chained fixups.
    ("core", ["headerpad.py"], {}, on("macos"), 1),
    # bun links with `-no_compact_unwind`, which leaves `__eh_frame` alone to the unwinder.
    ("core", ["nocompact.py"], {}, on("macos"), 1),
    # A branch reaches 128 MB on AArch64, and the splice has to get past that. Mach-O is not spliced.
    ("core", ["farsplices.py"], {"BLAZE_SPLICE": "1"}, on("linux-aarch64"), 1),
    # A file 2.3 GB past what it loads, as bevy's debug builds are; about 9 GB of disk at once.
    ("core", ["farfile.py"], {}, on("linux"), None),
    # 220 MB of code, 16 MB of frames and, on Linux, 4.2 GB of DWARF: no size is a full link.
    ("core", ["bigbinary.py"], {}, on("linux", "macos"), None),
    # `blaze` hands Windows builds to cargo, so there the daemon links nothing to test.
    ("core", ["unreached.py"], {}, on("linux", "macos"), 1),
    ("core", ["anonstatics.py"], {}, on("linux", "macos"), 1),
    # A static with state LLVM renamed (`SERVING.0`) at opt-level 1 is rustc's copy for both.
    ("core", ["loadedits.py"], {}, on("linux", "macos"), 2),
    # An edit of a dependency reaches each kind of artifact a build says it wrote, or the build
    # fails naming it (#730); a program built with #731's unstable rustflags has its edits spliced.
    ("core", ["artifactedits.py"], {}, on("linux", "macos"), 1),
    ("core", ["testselect.py"], {}, on("linux", "macos"), 2),
    # Each kind of reason a test is not hermetic, told from its code, and pure tests kept by it.
    ("core", ["hermetic.py"], {}, on("linux", "macos"), 2),
    ("core", ["doctestonce.py"], {}, on("linux", "macos"), 1),
    ("core", ["asmedits.py"], {}, EVERYWHERE, 1),
    # The assembler of a build of blaze without LLVM, which is the release's.
    ("core", ["asmedits.py"], {"BLAZE_ASSEMBLER": "cc"}, on("linux", "macos", backend="cranelift"), 1),
    ("core", ["includes.py", arg({})], {}, EVERYWHERE, 1),
    # A lazy static's initializer inlines a dependency's helper, which is then edited.
    ("core", ["staticcallbacks.py"], {}, on("linux", "macos"), None),
    ("core", ["latelisted.py"], {}, on("linux", "macos"), 1),
    ("core", ["rustsrc.py"], {}, EVERYWHERE, 1),
    # The release's library archive, made and served here, installed from by digest.
    ("core", ["libraryfetch.py"], {}, EVERYWHERE, 1),
    # Each scenario runs rustc and rustdoc no more often than norustc/baseline.json says.
    ("core", ["norustc.py"], {}, on("linux", "macos"), 2),
    # `blaze check`'s diagnostics, in each `--message-format`, against rustc's.
    ("core", ["check_json.py", arg({"blaze_bin": os.environ.get("BLAZE_BIN", "")})], {},
     on("linux", "macos", backend="cranelift"), 2),
    # Generated proc-macro input and writable LazyLock identity across edits and a restart.
    ("core", ["own_inputs.py", arg({"blaze": BLAZE_CLI})], {},
     on("linux", "macos", backend="cranelift"), None),
    ("core", ["checktargets.py"], {}, on("linux", "macos", backend="cranelift"), 2),
    ("core", ["nativespans.py"], {}, on("linux", "macos"), 2),
    # A proc macro calls C its library's build script compiled, for wasm32 by Ubuntu's clang.
    ("core", ["wasmcmacro.py"], {}, on("linux"), 2),
    ("core", ["gitindex.py"], {}, on("linux", "macos", backend="cranelift"), 2),
    ("core", ["deriveerrors.py"], {}, on("linux", "macos"), 2),
    # blaze answers what is asked of rustc as rustc answers it, on every target it knows.
    ("core", ["answers.py"], {}, on("linux", "macos", backend="cranelift"), 1),
    ("core", ["worktrees.py"], {}, EVERYWHERE, 2),
    ("core", ["worktrees.py", "--join", "--together"], {}, on("macos"), 2),
    ("core", ["worktrees.py", "--overlay"], {}, on("macos"), 2),
    # Tiers take live edits, which are macOS's alone; the harness picks its backends.
    ("core", ["tiers.py"], {}, on("macos", backend="cranelift"), 1),
    # Generated programs on fixed seeds, and generated edits under a daemon against cold builds.
    # Only x86-64 Linux has been fuzzed so far.
    ("core", ["fuzz/fuzz.py", "smoke", "--backends", BACKEND], {}, on("linux-x86_64"), 2),
    # #418: the same seeds with an empty and a warm store, and asm! text changed under one IR.
    ("core", ["fuzz/store.py", "--backends", BACKEND], {}, on("linux-x86_64"), 2),
    ("core", ["fuzz/edits.py", "smoke", arg({"backends": [BACKEND]})], {}, on("linux-x86_64"), 2),
    # Every reduced program fuzzing found blaze building wrong, on both architectures they
    # were found on; a part of its own, as it takes minutes.
    ("core", ["fuzz/fuzz.py", "check", "--backends", BACKEND], {}, on("linux"), 3),
    # How much of the corpus LLVM compiled itself, and what it left to Cranelift and why.
    ("core", ["backends.py", "corpus", "runner", "cases", "--bin", "runner", "--levels", "2f"], {}, on(backend="llvm"), 2),
    # DWARF read by gdb out of an ELF executable, or a PDB read by cdb.
    ("core", ["debuginfo.py"], {}, on("linux", "windows"), None),
    *((f"keys-{k}", ["keys.py", arg({"shard": {"index": k, "count": KEYS}})], {}, EVERYWHERE, None) for k in range(1, KEYS + 1)),
    # Builds for Linux targets besides the host; the cases of another host say so.
    ("buildmodel", ["buildmodel.py", "--backend", BACKEND], {}, on("linux", "windows"), None),
    ("store-kills", ["storekills.py", "--kills", "100"], {}, EVERYWHERE, None),
    ("store", ["storecorrupts.py", "--rounds", "4"], {}, EVERYWHERE, None),
    # It mounts a tmpfs, or fills a disk image on Windows. In the fast tier: what a build
    # keeps grows with features that write the store, and a full disk is where that shows.
    ("store", ["storefull.py"], {}, on("linux", "windows"), 2),
    ("store", ["storeshared.py"], {}, EVERYWHERE, None),
    ("store", ["storeguards.py"], {}, on("linux"), None),
    # A dependency's features and what its build script set and wrote, in the keys of its code.
    ("store", ["depkeys.py", arg({})], {}, on("linux"), None),
    ("store", ["storetracked.py"], {}, on("linux", "macos"), 2),
    ("store", ["storebuilds.py"], {}, on("linux", "macos"), 2),
    # Typed JSON from the Python workspace: the harness job has to run ci.py under `uv run --frozen`.
    ("store", ["storerevisions.py", json.dumps({"blaze_bin": os.environ.get("BLAZE_BIN", "")})], {},
     on("linux", "macos"), None),
    ("daemon", ["stress.py"], {}, EVERYWHERE, 2),
    ("daemon", ["selects.py"], {}, on("linux"), 2),
    # MinIO as one program in ~/s3tools, which CI puts there on Linux.
    ("daemon", ["remote.py"], {}, on("linux"), None),
]


# The fast tier: for each harness in it, the seconds it takes on the slowest host it runs on there
# (the median of 2026-10-26's merge-queue runs, the slower backend), and the hosts it runs on in
# the fast tier, where that is fewer than where it runs (no hosts: wherever it runs). A key is
# the start of a harness's label (`label`); the longest that fits is the harness's.
X64, MAC = "linux-x86_64", "macos"
FAST = {
    "difftest.py corpus": (34, ()),
    "programs.py": (20, ()),
    "BLAZE_ALONE=1 programs.py": (20, (X64,)),
    "breakers.py": (211, ()),
    "unwinds.py": (11, ()),
    "growth.py": (31, (X64,)),
    "coldbuild.py": (78, (X64,)),
    "stdstatics.py": (113, (X64, MAC)),
    "loadretry.py": (30, ()),
    "scriptenv.py": (70, ()),
    "splices.py": (51, ()),
    "live_e2e.py": (16, ()),
    "live_types.py": (9, ()),
    "LIVE_PROGRAM=live-alloc live_e2e.py": (16, ()),
    "headerpad.py": (18, ()),
    "nocompact.py": (12, ()),
    "BLAZE_SPLICE=1 farsplices.py": (8, ()),
    "unreached.py": (9, ()),
    "anonstatics.py": (9, ()),
    "loadedits.py": (19, (X64, MAC)),
    "artifactedits.py": (40, ()),
    "testselect.py": (58, (X64, MAC)),
    "hermetic.py": (15, (X64, MAC)),
    "doctestonce.py": (92, (X64,)),
    "asmedits.py": (12, ()),
    "BLAZE_ASSEMBLER=cc asmedits.py": (12, ()),
    "includes.py": (12, ()),
    "latelisted.py": (126, (X64,)),
    "rustsrc.py": (1, ()),
    # Not measured yet: library.py's xz of library/ (79 MB) is most of it.
    "libraryfetch.py": (90, (X64,)),
    "norustc.py": (253, (X64,)),
    "check_json.py": (38, (X64,)),
    "checktargets.py": (40, (X64,)),
    "nativespans.py": (90, (X64,)),
    # Not measured yet: two cargo and two blaze builds of three small crates, as nativespans.py.
    "wasmcmacro.py": (60, (X64,)),
    "gitindex.py": (3, (X64,)),
    "deriveerrors.py": (65, (X64,)),
    "answers.py": (17, (X64,)),
    "worktrees.py": (26, (X64, MAC)),
    "worktrees.py --join --together": (28, ()),
    "worktrees.py --overlay": (34, ()),
    "tiers.py": (87, ()),
    "fuzz/fuzz.py smoke": (60, ()),
    "fuzz/store.py": (120, ()),
    "fuzz/edits.py smoke": (62, ()),
    "fuzz/fuzz.py check": (259, ()),
    "backends.py corpus": (23, (X64,)),
    "storefull.py": (59, (X64,)),
    "storetracked.py": (33, (X64, MAC)),
    "storebuilds.py": (10, (X64, MAC)),
    "stress.py": (74, (X64,)),
    "selects.py": (29, (X64,)),
}
# What a fast part is packed to: with the build before it and the job's own set-up, about ten
# minutes from the start of the run.
PART_SECS = 240
# The full tier's part `core`, packed as the fast tier is: the seconds each harness took on the
# slowest host it runs on (the train's 2026-10-26 run 40283386868, the slower backend), keyed
# as FAST is. As one part it took 36 to 47 minutes on Linux and macOS, every other part 25 or
# less; wholeprogram.py alone is 19 minutes on macOS, and nothing packs a part below that.
# A harness CORE does not name counts CORE_DEFAULT.
CORE = {
    "wholeprogram.py": 1155,
    "debuginfo.py": 500,
    "bigbinary.py": 457,
    "fuzz/fuzz.py check": 264,
    "norustc.py": 229,
    "breakers.py": 189,
    "latelisted.py": 91,
    "splices.py": 89,
    "farfile.py": 84,
    "tiers.py": 79,
    "coldbuild.py": 62,
    "stdstatics.py": 113,
    "doctestonce.py": 54,
    "fuzz/edits.py smoke": 52,
    "testselect.py": 51,
    "fuzz/fuzz.py smoke": 41,
    "fuzz/store.py": 120,
    "difftest.py corpus": 34,
    "check_json.py": 32,
    "worktrees.py --overlay": 30,
    "worktrees.py": 27,
    "growth.py": 26,
    "backends.py corpus": 26,
    "worktrees.py --join --together": 26,
    "loadedits.py": 20,
}
CORE_DEFAULT = 15
CORE_SECS = 600
# The Ubuntu packages a part's harnesses use on Linux: valgrind shows where a program difftest
# built goes wrong, gdb the threads of a daemon that hangs.
APT = {"difftest.py": "valgrind", "worktrees.py": "gdb", "debuginfo.py": "gdb"}
BACKENDS = ("cranelift", "llvm")


def label(harness):
    _, args, env, _, _ = harness
    return " ".join([*(f"{k}={v}" for k, v in env.items()), *args])


def fast_of(harness):
    """(seconds, hosts) of a harness of the fast tier; an error for one FAST does not name."""
    name = label(harness)
    keys = [key for key in FAST if name == key or name.startswith(key + " ")]
    if not keys:
        sys.exit(f"ci.py: {name!r} is in the fast tier and FAST has no line for it: add its seconds "
                 f"on the slowest host, and the hosts it runs on in the fast tier")
    return FAST[max(keys, key=len)]


def check_tables():
    """Stops at a line of FAST or CORE that names no harness: one left behind when its harness
    went (aheadready.py and renamedstatics.py after #408) times nothing, and says nothing."""
    labels = [label(harness) for harness in HARNESSES]
    for table, name in ((FAST, "FAST"), (CORE, "CORE")):
        for key in table:
            if not any(it == key or it.startswith(key + " ") for it in labels):
                sys.exit(f"ci.py: {name} has a line for {key!r}, which no harness in HARNESSES is: "
                         f"take it out with its harness")


def fast_here(backend):
    """The fast tier's harnesses on this machine under `backend`, in their order."""
    chosen = []
    for harness in HARNESSES:
        if harness[4] is not None and harness[3](backend):
            secs, hosts = fast_of(harness)
            if here(hosts):
                chosen.append((harness, secs))
    return chosen


def pack(chosen, secs_per_part, prefix):
    """[(harness, seconds), ...] packed into parts of about `secs_per_part` (the longest first
    into the part with the least so far): {prefix-N: [harness, ...]}, each in HARNESSES' order."""
    longest = max((secs for _, secs in chosen), default=0)
    total = sum(secs for _, secs in chosen)
    # As many parts as the total needs, but no more than leave the longest harness alone in one.
    count = max(1, math.ceil(total / max(secs_per_part, longest))) if chosen else 0
    loads, members = [0] * count, [[] for _ in range(count)]
    for index, (_, secs) in sorted(enumerate(chosen), key=lambda it: (-it[1][1], it[0])):
        part = loads.index(min(loads))
        loads[part] += secs
        members[part].append(index)
    return {f"{prefix}-{n + 1}": [chosen[i][0] for i in sorted(members[n])] for n in range(count)}


def fast_parts(backend):
    """The fast tier here under `backend`, in parts of about PART_SECS."""
    return pack(fast_here(backend), PART_SECS, "quick")


def core_secs(harness):
    name = label(harness)
    keys = [key for key in CORE if name == key or name.startswith(key + " ")]
    return CORE[max(keys, key=len)] if keys else CORE_DEFAULT


def full_parts(backend):
    """The full tier here under `backend`: its named parts, and `core` in parts of about
    CORE_SECS (core-1 ... core-N)."""
    parts, core = {}, []
    for harness in HARNESSES:
        if harness[3](backend):
            if harness[0] == "core":
                core.append((harness, core_secs(harness)))
            else:
                parts.setdefault(harness[0], []).append(harness)
    return {**pack(core, CORE_SECS, "core"), **parts}


def parts(tier="full"):
    """CI's matrix here: an entry per backend and part, with the Ubuntu packages it needs."""
    include = []
    for backend in BACKENDS:
        for part, harnesses in (fast_parts if tier == "fast" else full_parts)(backend).items():
            apt = sorted({APT[h[1][0]] for h in harnesses if h[1][0] in APT}) if SYSTEM == "linux" else []
            include.append({"backend": backend, "part": part, "apt": " ".join(apt)})
    return {"include": include}


def run_one(args, env):
    """Runs one harness with a daemon home of its own, kept when it fails (CI shows the daemon
    logs under /tmp then). BLAZE_CACHE stays as the caller has it: set, it turns the store on,
    which only the store harnesses do."""
    # Under /tmp, short: a daemon's socket is under its home, and macOS allows 104 bytes.
    home = tempfile.mkdtemp(prefix="rj-ci-", dir="/tmp" if SYSTEM != "windows" else None)
    own = {"BLAZE_HOME": home}
    status = subprocess.run([sys.executable, *args], cwd=HERE, env={**os.environ, **own, **env}, check=False).returncode
    # A daemon the harness left running in that home, which nothing after it may talk to.
    blaze_cli = os.path.join(os.path.dirname(os.path.abspath(os.environ.get("BLAZE_BIN", ""))), "blaze")
    if os.environ.get("BLAZE_BIN") and os.path.exists(blaze_cli + (".exe" if SYSTEM == "windows" else "")):
        subprocess.run([blaze_cli, "blaze", "stop"], env={**os.environ, **own}, capture_output=True,
                       timeout=120, check=False)
    if status == 0:
        shutil.rmtree(home, ignore_errors=True)
    return status


def run(part):
    tier = "fast" if part.startswith("quick-") else "full"
    if part == "fast":
        tier, chosen = "fast", [(h[1], h[2]) for h, _ in fast_here(BACKEND)]
    else:
        chosen = [(h[1], h[2]) for h in (fast_parts if tier == "fast" else full_parts)(BACKEND).get(part, [])]
    if not chosen:
        known = list((fast_parts if tier == "fast" else full_parts)(BACKEND))
        sys.exit(f"ci.py: no harness of part {part!r} runs on {SYSTEM} {ARCH} with {BACKEND}; the parts here are {known}")
    os.environ["BLAZE_CI_TIER"] = tier
    failed, times = [], []
    for args, env in chosen:
        what = " ".join([*(f"{k}={v}" for k, v in env.items()), *args])
        print(f"::group::{what}", flush=True)
        start = time.monotonic()
        status = run_one(args, env)
        times.append((time.monotonic() - start, what))
        print("::endgroup::", flush=True)
        if status:
            failed.append(what)
            print(f"::error::{what} failed (exit status {status})", flush=True)
    for seconds, what in times:
        print(f"{seconds:7.0f} s  {what}")
    print(f"failed: {', '.join(failed) or 'none'}")
    sys.exit(1 if failed else 0)


SCRIPT = "ci.py"


@dataclass(frozen=True)
class Run:
    # A part of `parts`, or `fast` for the whole fast tier here.
    part: str


@dataclass(frozen=True)
class Parts:
    tier: Literal["full", "fast"] = "full"


@dataclass(frozen=True)
class List:
    """No field."""


def main():
    check_tables()
    match sys.argv[1:]:
        case ["run", text]:
            run(parse(Run, text, SCRIPT).part)
        case ["parts", text]:
            print(json.dumps(parts(parse(Parts, text, SCRIPT).tier)))
        case ["list", text]:
            parse(List, text, SCRIPT)
            for harness in HARNESSES:
                fast = ""
                if harness[4] is not None:
                    secs, hosts = fast_of(harness)
                    fast = f"fast {secs:3d} s {','.join(hosts) or 'all'}"
                part = f"core {core_secs(harness)} s" if harness[0] == "core" else harness[0]
                print(f"{part:12} {fast:28} {label(harness)}")
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} run|parts|list JSON (see the top of the file)")


if __name__ == "__main__":
    main()
