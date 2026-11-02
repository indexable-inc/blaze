# pyright: basic
"""ci.py: the harnesses CI runs, in parts that take about the same time on a hosted runner.

usage: ci.py run '{"part": "core-1"}'   run the harnesses of a part on this machine
       ci.py run '{"part": "fast"}'     run every harness of the fast tier here, one after another
       ci.py parts '{"tier": "full"}'   CI's matrix for this machine, as JSON: {"include": [{"part",
                                        "apt"}, ...]}, tier `full` (the default) or `fast`
       ci.py list '{}'                  which harness runs in which part, on which hosts

After the subcommand comes one JSON object (inline, the path of a file, or `-` for standard
input) with the fields of its class below (Run, Parts, List); a field it does not have is
refused. Run it in the repository's Python workspace, `uv run --frozen python harness/ci.py ...`
from the root: the harnesses run in the Python that runs it, and some read their input with the
workspace's typed_input.

There is no backend to choose: v2 picks it by the profile (opt-level 0 and 1 with Cranelift, the
rest with LLVM, crates/blaze-cargo/src/serve/unit/codegen.rs) and refuses every unit while
BLAZE_BACKEND or BLAZE_OPT is set (crates/hir-ty-codegen/v2/backend.rs, CHANGES_CODE). A
harness that is to cover LLVM builds the release profile (the fuzzers' `profiles`,
`wholeprogram.py --release`).

The fast tier, of every pull request and merge-queue entry, is the harnesses that FAST names,
each on the hosts FAST gives it: on Linux x86-64 all of them, and on Linux AArch64 and macOS the
ones where the platform matters (code generation and the ABI, the object format and the linker,
the sandbox, live edits, the store's files). They are packed into parts `quick-1` ... `quick-N`
of about PART_SECS each, per host, by the seconds FAST says each takes on the
slowest host, so a new or slower harness moves the others, not the time of the tier. The full
tier, the release train's (every push to main) and by hand, is every harness, in the parts
core-1 ... core-N (packed as the fast tier is, to about CORE_SECS by the seconds CORE says),
keys-N, store-kills, store and daemon (docs/harness.md, "CI").

Each harness runs with a BLAZE_HOME of its own (a harness may set its own over it), so no
daemon, socket or store (`<home>/v2/store`) of one harness is seen by the next.

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
BLAZE_CLI = os.path.join(
    os.path.dirname(os.path.abspath(os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine"))),
    "blaze.exe" if SYSTEM == "windows" else "blaze",
)


def here(hosts):
    """Whether this machine is one of `hosts` ("linux", "macos", "windows", "linux-aarch64"...);
    no hosts is every one."""
    return not hosts or SYSTEM in hosts or f"{SYSTEM}-{ARCH}" in hosts


def arg(value):
    """A harness's one typed input, as short as JSON writes it: it is the harness's label too."""
    return json.dumps(value, separators=(",", ":"))


def on(*hosts):
    """Where a harness runs: the hosts `here` takes."""
    return hosts


EVERYWHERE = on()
# What builds with the v2 daemon (rebuild.py, storelib.py): its CLI speaks over a Unix socket, so
# not Windows.
DAEMON = on("linux", "macos")
KEYS = 3
# (part in the full tier, arguments, environment, where it runs, in the fast tier: None or not).
# The order is the order of each part's run. A harness in the fast tier needs a line in FAST.
HARNESSES = [
    # An edit gives the program a fresh build gives (rebuild.py): two programs of one workspace.
    ("core", ["programs.py"], {}, DAEMON, 1),
    ("core", ["unwinds.py"], {}, DAEMON, 1),
    ("core", ["growth.py"], {}, DAEMON, 1),
    # 11 to 22 minutes a run under v1, longer than the rest of the fast tier, and it caught
    # nothing there in 2026-10-25 and 26's 270 runs: the full tier's alone.
    ("core", ["wholeprogram.py"], {}, DAEMON, None),
    # The release profile, which v2 compiles with LLVM.
    ("core", ["wholeprogram.py", "--release"], {}, DAEMON, None),
    # `blaze build` from nothing: G's whole build, no cargo build.
    ("core", ["coldbuild.py"], {}, on("linux"), 2),
    # A load refused for configuration must be retried without stopping the daemon (#597).
    ("core", ["loadretry.py"], {}, on("linux", "macos"), 2),
    # A build script reads the command's environment, not the daemon's (#720).
    ("core", ["scriptenv.py"], {}, on("linux", "macos"), 2),
    # Its program of writable statics of its own crates on macOS too, where anim's `HOME` was refused.
    ("core", ["coldbuild.py", "statics"], {}, on("macos"), None),
    ("core", ["live_e2e.py"], {}, on("linux-x86_64"), 1),
    # A type added beside another, the code moved to it and the old one deleted, each a patch.
    ("core", ["live_types.py"], {}, on("linux-x86_64"), 1),
    # A program on a nightly feature (allocator_api) builds and takes live edits the same way.
    ("core", ["live_e2e.py"], {"LIVE_PROGRAM": "live-alloc"}, on("linux-x86_64"), 1),
    # Apple's linker leaves no more room than it is told, and gives a newer macOS chained fixups.
    ("core", ["headerpad.py"], {}, on("macos"), 1),
    # bun links with `-no_compact_unwind`, which leaves `__eh_frame` alone to the unwinder.
    ("core", ["nocompact.py"], {}, on("macos"), 1),
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
    # A lazy static's initializer inlines a dependency's helper, which is then edited.
    ("core", ["staticcallbacks.py"], {}, on("linux", "macos"), None),
    ("core", ["latelisted.py"], {}, on("linux", "macos"), 1),
    ("core", ["includes.py", arg({})], {}, DAEMON, 1),
    ("core", ["rustsrc.py"], {}, DAEMON, 1),
    # The release's library archive, made and served here, installed from by digest.
    ("core", ["libraryfetch.py"], {}, EVERYWHERE, 1),
    # Each scenario runs rustc and rustdoc no more often than norustc/baseline.json says.
    ("core", ["norustc.py"], {}, on("linux", "macos"), 2),
    # Generated proc-macro input and writable LazyLock identity across edits and a restart.
    ("core", ["own_inputs.py", arg({"blaze": BLAZE_CLI})], {}, on("linux", "macos"), None),
    # A changed C file of a build script under a warm daemon builds as from nothing.
    ("core", ["nativespans.py"], {}, on("linux", "macos"), 2),
    # A proc macro calls C its library's build script compiled, for wasm32 by Ubuntu's clang.
    ("core", ["wasmcmacro.py"], {}, on("linux"), 2),
    # A derive on what it cannot derive stops with the error the case names.
    ("core", ["deriveerrors.py"], {}, on("linux", "macos"), 2),
    ("core", ["worktrees.py"], {}, DAEMON, 2),
    ("core", ["worktrees.py", "--together"], {}, on("macos"), 2),
    # Tiers take live edits, which are macOS's alone.
    ("core", ["tiers.py"], {}, on("macos"), 1),
    # #418: the same seeds with an empty and a warm store, and asm! text changed under one IR;
    # each profile picks its backend (dev: Cranelift, release: LLVM).
    ("core", ["fuzz/store.py", arg({"blaze": BLAZE_CLI, "profiles": ["dev", "release"]})], {}, on("linux-x86_64"), 2),
    ("core", ["fuzz/edits.py", "smoke", arg({"profiles": ["dev", "release"]})], {}, on("linux-x86_64"), 2),
    # DWARF read by gdb out of an ELF executable.
    ("core", ["debuginfo.py"], {}, on("linux"), None),
    *((f"keys-{k}", ["keys.py", arg({"shard": {"index": k, "count": KEYS}})], {}, DAEMON, None) for k in range(1, KEYS + 1)),
    # The v2 daemon's store (crates/blaze-cas): its CLI speaks over a Unix socket, so not Windows.
    ("store-kills", ["storekills.py", arg({"blaze": BLAZE_CLI, "kills": 100})], {}, on("linux", "macos"), None),
    ("store", ["storecorrupts.py", arg({"blaze": BLAZE_CLI, "rounds": 4})], {}, on("linux", "macos"), None),
    # It mounts a tmpfs. In the fast tier: what a build keeps grows with features that write
    # the store, and a full disk is where that shows.
    ("store", ["storefull.py", arg({"blaze": BLAZE_CLI})], {}, on("linux"), 2),
    ("store", ["storeguards.py", arg({"blaze": BLAZE_CLI})], {}, on("linux", "macos"), None),
    ("store", ["storetracked.py", arg({"blaze": BLAZE_CLI})], {}, on("linux", "macos"), 2),
    ("store", ["storerevisions.py", arg({"blaze": BLAZE_CLI})], {}, on("linux", "macos"), None),
    ("daemon", ["stress.py"], {}, DAEMON, 2),
    ("daemon", ["selects.py"], {}, on("linux"), 2),
]


# The fast tier: for each harness in it, the seconds it takes on the slowest host it runs on there
# (the median of 2026-10-26's merge-queue runs, the slower backend of v1's matrix), and the hosts it runs on in
# the fast tier, where that is fewer than where it runs (no hosts: wherever it runs). A key is
# the start of a harness's label (`label`); the longest that fits is the harness's.
X64, MAC = "linux-x86_64", "macos"
FAST = {
    "coldbuild.py": (78, (X64,)),
    "loadretry.py": (30, ()),
    "scriptenv.py": (70, ()),
    "live_e2e.py": (16, ()),
    "live_types.py": (9, ()),
    "LIVE_PROGRAM=live-alloc live_e2e.py": (16, ()),
    "headerpad.py": (18, ()),
    "nocompact.py": (12, ()),
    "unreached.py": (9, ()),
    "anonstatics.py": (9, ()),
    "loadedits.py": (19, (X64, MAC)),
    "artifactedits.py": (40, ()),
    "testselect.py": (58, (X64, MAC)),
    "hermetic.py": (15, (X64, MAC)),
    "doctestonce.py": (92, (X64,)),
    "latelisted.py": (126, (X64,)),
    # Not measured yet: library.py's xz of library/ (79 MB) is most of it.
    "libraryfetch.py": (90, (X64,)),
    "norustc.py": (253, (X64,)),
    "nativespans.py": (90, (X64,)),
    # Not measured yet: two cargo and two blaze builds of three small crates, as nativespans.py.
    "wasmcmacro.py": (60, (X64,)),
    "deriveerrors.py": (65, (X64,)),
    "tiers.py": (87, ()),
    # Each profile was a run of its own under one backend of v1's matrix: both, about twice that.
    "fuzz/store.py": (240, ()),
    "storefull.py": (59, (X64,)),
    "storetracked.py": (33, (X64, MAC)),
    # v1's seconds, from before these were ported onto `blaze build` (rebuild.py): not measured
    # under v2 yet.
    "programs.py": (20, ()),
    "unwinds.py": (11, ()),
    "growth.py": (31, (X64,)),
    "includes.py": (12, ()),
    "rustsrc.py": (1, ()),
    "worktrees.py": (26, (X64, MAC)),
    "worktrees.py --together": (28, ()),
    "fuzz/edits.py smoke": (124, ()),
    "stress.py": (74, (X64,)),
    "selects.py": (29, (X64,)),
}
# What a fast part is packed to: with the build before it and the job's own set-up, about ten
# minutes from the start of the run.
PART_SECS = 240
# The full tier's part `core`, packed as the fast tier is: the seconds each harness took on the
# slowest host it runs on (the train's 2026-10-26 run 40283386868, the slower backend of v1's matrix), keyed
# as FAST is. As one part it took 36 to 47 minutes on Linux and macOS, every other part 25 or
# less. A harness CORE does not name counts CORE_DEFAULT.
CORE = {
    "norustc.py": 229,
    "latelisted.py": 91,
    "tiers.py": 79,
    "coldbuild.py": 62,
    "doctestonce.py": 54,
    "testselect.py": 51,
    "fuzz/store.py": 240,
    "loadedits.py": 20,
    # v1's seconds, as in FAST: wholeprogram.py alone was 19 minutes on macOS.
    "wholeprogram.py": 1155,
    "debuginfo.py": 500,
    "fuzz/edits.py smoke": 104,
    "worktrees.py": 27,
    "growth.py": 26,
    "worktrees.py --together": 26,
}
CORE_DEFAULT = 15
CORE_SECS = 600
# The Ubuntu packages a part's harnesses use on Linux: gdb shows the threads of a daemon that
# hangs, and debuginfo.py steps through a program with it.
APT = {"worktrees.py": "gdb", "debuginfo.py": "gdb"}


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


def fast_here():
    """The fast tier's harnesses on this machine, in their order."""
    chosen = []
    for harness in HARNESSES:
        if harness[4] is not None and here(harness[3]):
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


def fast_parts():
    """The fast tier here, in parts of about PART_SECS."""
    return pack(fast_here(), PART_SECS, "quick")


def core_secs(harness):
    name = label(harness)
    keys = [key for key in CORE if name == key or name.startswith(key + " ")]
    return CORE[max(keys, key=len)] if keys else CORE_DEFAULT


def full_parts():
    """The full tier here: its named parts, and `core` in parts of about CORE_SECS (core-1 ...
    core-N)."""
    parts, core = {}, []
    for harness in HARNESSES:
        if here(harness[3]):
            if harness[0] == "core":
                core.append((harness, core_secs(harness)))
            else:
                parts.setdefault(harness[0], []).append(harness)
    return {**pack(core, CORE_SECS, "core"), **parts}


def parts(tier="full"):
    """CI's matrix here: an entry per part, with the Ubuntu packages it needs."""
    include = []
    for part, harnesses in (fast_parts if tier == "fast" else full_parts)().items():
        apt = sorted({APT[h[1][0]] for h in harnesses if h[1][0] in APT}) if SYSTEM == "linux" else []
        include.append({"part": part, "apt": " ".join(apt)})
    return {"include": include}


def run_one(args, env):
    """Runs one harness with a daemon home of its own, kept when it fails (CI shows the daemon
    logs under /tmp then)."""
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
        tier, chosen = "fast", [(h[1], h[2]) for h, _ in fast_here()]
    else:
        chosen = [(h[1], h[2]) for h in (fast_parts if tier == "fast" else full_parts)().get(part, [])]
    if not chosen:
        known = list((fast_parts if tier == "fast" else full_parts)())
        sys.exit(f"ci.py: no harness of part {part!r} runs on {SYSTEM} {ARCH}; the parts here are {known}")
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
