#!/usr/bin/env python3
"""Differential test: build a binary with rustc, rebuild chosen crates with ra-jit, compare behaviour.

usage: difftest.py <workspace> --bin <package> --crate <name> [--crate <name> ...] [--bisect]

Exit code 0 when stdout, stderr and exit status match. With --bisect, each ra-jit function is
also linked in on its own to name the ones that change behaviour.

RA_JIT_TARGET=wasm32-wasip1 builds both sides for that target and runs them under wasmtime
(RA_JIT_WASM_RUNNER names another runner); the other harnesses take it the same way. See
WASM32.md.
"""
import re
import argparse, glob, json, os, shlex, subprocess, sys, tempfile, time
import linkline

# ra-jit computes layouts with rust-analyzer's copy of rustc_abi, so everything it links against
# has to come from the rustc that copy was taken from.
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolchain")).read().strip())

# rustc makes what only one codegen unit uses local to it unless it is told to keep unused code.
# ra-jit needs the symbols: a private `static mut` has to be the one object for both compilers.
os.environ.setdefault("RUSTFLAGS", "-C link-dead-code")
# Incremental caches of a sweep over many crates fill the disk.
os.environ.setdefault("CARGO_INCREMENTAL", "0")
# Set, not unset: cargo's `[env]` (as in ~/.cargo/config.toml) fills in only what is not already
# set, so an unset RUST_BACKTRACE would take its value and panics would print differently.
os.environ["RUST_BACKTRACE"] = "0"

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# A sweep runs against a copy, so rebuilding ra-jit meanwhile does not change it under the sweep.
WINDOWS = os.name == "nt"
RA_JIT = os.environ.get("RA_JIT_BIN") or os.path.join(ROOT, "target/release/ra-jit" + (".exe" if WINDOWS else ""))


def host():
    return subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].split()[0]


# RA_JIT_TARGET names the triple to build for when it is not the host. Cargo builds for it, ra-jit
# emits for it, and the binaries run under what RA_JIT_RUNNER names (for aarch64 Linux on another
# machine, qemu's user mode with the sysroot the cross linker links against).
TARGET = os.environ.get("RA_JIT_TARGET") or None
CROSS = {
    "aarch64-unknown-linux-gnu": {"linker": "aarch64-linux-gnu-gcc", "runner": "qemu-aarch64 -L /usr/aarch64-linux-gnu",
                                  "binutils": "aarch64-linux-gnu-"},
}
if TARGET and TARGET != host():
    _cross, _env = CROSS.get(TARGET, {}), "CARGO_TARGET_" + TARGET.upper().replace("-", "_")
    if "linker" in _cross:
        os.environ.setdefault(_env + "_LINKER", _cross["linker"])
    if "runner" in _cross:
        os.environ.setdefault("RA_JIT_RUNNER", _cross["runner"])
        os.environ.setdefault(_env + "_RUNNER", os.environ["RA_JIT_RUNNER"])
if TARGET:
    # Every cargo a harness runs builds for the target, and so does every ra-jit.
    os.environ["CARGO_BUILD_TARGET"] = TARGET
# What the names of strip and the like begin with for the target.
BINUTILS = os.environ.get("RA_JIT_BINUTILS") or (CROSS.get(TARGET, {}).get("binutils", "") if TARGET and TARGET != host() else "")
# What a built binary is started with.
RUNNER = shlex.split(os.environ.get("RA_JIT_RUNNER", ""))
# What cargo and ra-jit take to build for the target.
TARGET_ARGS = ["--target", TARGET] if TARGET else []


def profile_dir(target_dir, profile="debug"):
    """Where cargo puts what it builds under a target directory."""
    return os.path.join(target_dir, *([TARGET] if TARGET else []), profile)


def toolchain():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    return sysroot, host()


def linker_line(output):
    """The line of `--print link-args`, as the arguments from the linker on: the C compiler
    where it drives the linker, and a linker of the `link.exe` kind (rustc's own `rust-lld`,
    or MSVC's) for the MSVC target."""
    for line in output.splitlines():
        if '"' not in line:
            continue
        try:
            args = shlex.split(line)
        except ValueError:
            continue
        if "-o" in args:
            # The environment the driver runs in comes first, as assignments.
            link = args[next(i for i, a in enumerate(args) if "=" not in a):]
            if link[0] == "rust-lld":
                # The linker of a wasm target, which rustc finds in what it puts first in `PATH`.
                path = next(a for a in args if a.startswith("PATH=")).removeprefix("PATH=")
                link[0] = next(p for d in path.split(os.pathsep) if os.path.exists(p := os.path.join(d, link[0])))
            return link
        for at, arg in enumerate(args[:8]):
            if os.path.basename(arg).lower().removesuffix(".exe") in ("rust-lld", "lld-link", "link") and "/NOLOGO" in args:
                if arg == "rust-lld":
                    # rustc finds its own linker beside itself rather than on PATH.
                    sysroot, triple = toolchain()
                    arg = os.path.join(sysroot, "lib", "rustlib", triple, "bin", "rust-lld.exe")
                return [arg, *args[at + 1:]]
    raise SystemExit(f"no linker line:\n{output[-3000:]}")


# `wasm32-wasip1` runs under wasmtime.
WASM = bool(TARGET) and TARGET.startswith("wasm32")
if WASM and not RUNNER:
    # What runs a wasm program, which gets the program and its arguments after it.
    RUNNER = shlex.split(os.environ.get("RA_JIT_WASM_RUNNER", "wasmtime run -W max-wasm-stack=8388608 -S inherit-env"))
# What a link line gets after ra-jit's objects, which define again what rustc's do.
AHEAD = linkline.allowing_duplicates(WASM)


def command(binary):
    """What runs a built program."""
    return [*RUNNER, binary]


def built(target_dir, name):
    """Where cargo leaves the program `name`."""
    return os.path.join(profile_dir(target_dir), name + (".wasm" if WASM else ".exe" if WINDOWS else ""))


def sysroot_rlibs():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    lib = f"{sysroot}/lib/rustlib/{TARGET or host()}/lib"
    return [p for n in ("core", "alloc", "std") for p in glob.glob(f"{lib}/lib{n}-*.rlib")]


def label(crate):
    """A crate may be given as its root file, and several that one run compiles with commas."""
    return "+".join(os.path.basename(c).removesuffix(".rs") for c in crate.split(","))


class Fixture:
    def __init__(self, workspace, crates, target, touch=None):
        """`target` selects what cargo links: ["-p", "app"] or ["--test", "name"]."""
        self.ws, self.crates, self.target_args = os.path.abspath(workspace), crates, target
        self.work = tempfile.mkdtemp(prefix="ra-jit-diff-")
        self.run_args = []
        self.ra_jit_args = []
        self.ra_jit_timeout = None
        self.layout_log = os.path.join(self.work, "layouts.tsv")
        if touch is None:
            meta = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=self.ws))
            touch = next(t["src_path"] for p in meta["packages"] if p["name"] == target[1]
                         for t in p["targets"] if "bin" in t["kind"])
        self.touch = touch

    def rustc_build(self):
        """Builds with cargo and captures the linker line so ra-jit objects can be spliced in."""
        os.utime(self.touch)
        env, target = dict(os.environ), []
        # In the workspace's own target directory: cargo names a package's artifacts by its
        # path in its workspace, so programs of one name built at once in the caller's would
        # take each other's place (fuzz/edits.py's, all of them `fz`).
        env.pop("CARGO_TARGET_DIR", None)
        if TARGET:
            target = TARGET_ARGS
        out = subprocess.run(
            ["cargo", "rustc", *self.target_args, *target, "--", "--print", "link-args", "-C", "save-temps"],
            cwd=self.ws, capture_output=True, text=True, env=env)
        if out.returncode != 0:
            raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
        # rustc's object of the names the program keeps is how GNU ld, reading each archive once,
        # takes the panic runtime that comes before the standard library calling it.
        self.link_args = [a for a in linker_line(out.stdout + out.stderr)
                          if not a.endswith("symbols.o") or os.path.exists(a)]
        self.reference = linkline.output_of(self.link_args)
        # The target's own objects too: a crate ra_jit rebuilds may be the one being linked.
        self.rlibs = [a for a in self.link_args if a.endswith((".rlib", ".rcgu.o"))]

    def ra_jit_build(self, deny=()):
        """Compiles each crate with ra-jit and links the objects ahead of rustc's rlibs."""
        deny_file = os.path.join(self.work, "deny.txt")
        open(deny_file, "w").write("\n".join(deny))
        symbols = [x for rlib in self.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        objects, self.reports = [], {}
        for crate in self.crates:
            obj, report = (os.path.join(self.work, f"{label(crate)}.{ext}") for ext in ("o", "tsv"))
            try:
                run = subprocess.run(
                    [RA_JIT, self.ws, *(x for c in crate.split(",") for x in ("--crate", c)), "--out", obj, "--report", report,
                     "--deny", deny_file, "--isolate", os.path.join(self.work, f"{label(crate)}.isolated"),
                     *self.ra_jit_args, *TARGET_ARGS, *symbols],
                    capture_output=True, text=True, timeout=self.ra_jit_timeout,
                    env={**os.environ, "RA_JIT_LAYOUTS": self.layout_log})
            except subprocess.TimeoutExpired:
                raise SystemExit(f"ra-jit crashed on {crate}:\nerror: no result after {self.ra_jit_timeout}s")
            open(os.path.join(self.work, f"{label(crate)}.stderr"), "w").write(run.stderr)
            if os.environ.get("RA_JIT_STDERR"): open(os.environ["RA_JIT_STDERR"], "a").write(run.stderr)
            if run.returncode != 0:
                raise SystemExit(f"ra-jit crashed on {crate}:\n{run.stderr[-2000:]}")
            objects.append(obj)
            self.reports[crate] = [l.rstrip("\n").split("\t") for l in open(report)]
        self.objects = objects
        self.link(objects)

    def link(self, objects):
        args = list(self.link_args)
        args = linkline.ahead(args, objects, RA_JIT)
        self.binary = self.binary_path()
        args = linkline.with_output(args, self.binary)
        run = subprocess.run(args, capture_output=True, text=True)
        if run.returncode != 0:
            raise SystemExit(f"link failed:\n{(run.stderr + run.stdout)[-20000:]}")

    def binary_path(self):
        """Where the binary with ra-jit's objects is linked."""
        return os.path.join(self.work, "bin-ra-jit")

    def compiled(self):
        return [r[1] for rows in self.reports.values() for r in rows if r[0] == "compiled"]

    def behaviour(self, binary):
        # A loaded machine slows both builds alike, so rustc's own run sets the allowance.
        allowed = 900 if binary == self.reference else max(120, 30 * getattr(self, "reference_secs", 0))
        start = time.monotonic()
        try:
            run = subprocess.run([*RUNNER, binary, *self.run_args], capture_output=True, timeout=allowed)
            if binary == self.reference:
                self.reference_secs = time.monotonic() - start
            # The thread id in a panic or stack overflow message differs from run to run.
            stderr = re.sub(rb"' \(\d+\) (panicked|has overflowed)", rb"' \1", run.stderr)
            if WASM:
                # After the program's own output the runtime says where in which file it
                # trapped: only the kind of trap is the program's.
                program, _, runtime = stderr.partition(b"Error: failed to run main module")
                stderr = program + b"".join(re.findall(rb"wasm trap: [^\n]*\n?", runtime))
            # Loggers stamp their lines with the time, on either stream.
            stamp = rb"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?"
            # What prints an address prints another one in every run.
            address = rb"0x[0-9a-f]{12}\b"
            stdout, stderr = (
                re.sub(address, b"<address>", re.sub(stamp, b"<time>", it)) for it in (run.stdout, stderr)
            )
            return run.returncode, self.normalize(stdout), stderr
        except subprocess.TimeoutExpired:
            return "timeout", b"", b""

    def normalize(self, stdout):
        return stdout

    def matches(self, expected):
        return self.behaviour(self.binary) == expected


def isolate(fx, expected):
    """Links each ra-jit function into the rustc binary on its own; returns the ones that change behaviour."""
    culprits = []
    for crate in fx.crates:
        directory = os.path.join(fx.work, f"{label(crate)}.isolated")
        for line in open(os.path.join(directory, "index.tsv")):
            index, name = line.rstrip("\n").split("\t")
            fx.link([os.path.join(directory, f"{index}.o")])
            if not fx.matches(expected):
                got = fx.behaviour(fx.binary)
                culprits.append({"function": name, "symptom": symptom(expected, got)})
    return culprits


def minimize(items, fails):
    """Delta debugging: a smaller list for which `fails` still holds."""
    n = 2
    while len(items) >= 2:
        size = max(len(items) // n, 1)
        chunks = [items[i:i + size] for i in range(0, len(items), size)]
        for i, chunk in enumerate(chunks):
            rest = [x for j, c in enumerate(chunks) if j != i for x in c]
            if fails(chunk):
                items, n = chunk, 2
                break
            if len(chunks) > 2 and fails(rest):
                items, n = rest, max(n - 1, 2)
                break
        else:
            if size == 1:
                break
            n = min(n * 2, len(items))
    return items


def symptom(expected, got):
    if got[0] != expected[0]:
        return f"exit status {got[0]}"
    want, have = expected[1].decode(errors="replace").splitlines(), got[1].decode(errors="replace").splitlines()
    for w, h in zip(want, have):
        if w != h:
            return f"expected `{w}`, got `{h}`"
    return "output length differs"


def bucket(error):
    """Failure reason without the payload, so the same gap in different functions counts once."""
    import re
    reason = error.rsplit("`: ", 1)[-1]
    reason = re.sub(r"^(unsupported|frontend|backend): ", "", reason)
    for prefix in ("drop glue", "unsizing", "aggregate of", "expected scalar", "cast "):
        if reason.startswith(prefix):
            return {"aggregate of": "odd-sized aggregate in a register", "cast ": reason.split("(")[-1].rstrip(")") + " cast"}.get(prefix, prefix)
    return re.split(r"[({]", reason)[0].strip()[:60]


def memcheck(fx):
    """Runs the ra-jit binary under valgrind: the first invalid access is usually the miscompile,
    where the crash or wrong output is only what it led to. Returns the error blocks."""
    import re, shutil
    # Valgrind runs what the machine runs, not what an emulator does.
    if RUNNER or not shutil.which("valgrind"):
        return None
    log = os.path.join(fx.work, "valgrind.log")
    try:
        subprocess.run(["valgrind", "-q", "--num-callers=30", f"--log-file={log}", fx.binary, *fx.run_args],
                       capture_output=True, timeout=900)
    except subprocess.TimeoutExpired:
        pass
    text = re.sub(r"^==\d+== ?", "", open(log).read(), flags=re.M)
    errors = [e for e in text.split("\n\n") if e.strip()]
    print(f"  valgrind: {len(errors)} errors ({log})")
    if errors:
        print("\n".join("    " + line[:200] for line in errors[0].splitlines()))
    return errors


def compare(fx, name, do_bisect, do_memcheck=False, do_layouts=False, mixed=0):
    fx.rustc_build()
    expected = fx.behaviour(fx.reference)
    fx.ra_jit_build()
    ok = fx.matches(expected)
    result = {"fixture": name, "match": ok, "crates": {}}
    for crate, rows in fx.reports.items():
        n = sum(r[0] == "compiled" for r in rows)
        total = sum(r[0] in ("compiled", "skipped") for r in rows)
        reasons = {}
        for r in rows:
            if r[0] == "skipped":
                key = bucket(r[2])
                reasons[key] = reasons.get(key, 0) + 1
        no_symbol = reasons.pop("no rustc symbol for function", 0)
        result["crates"][crate] = {
            "compiled": n, "roots": total - no_symbol, "not_roots": no_symbol,
            "instances": int(rows[0][2]), "codegen_ms": int(rows[0][4]) / 1000, "skipped": reasons}
        print(f"{label(crate)}: {n}/{total - no_symbol} functions from ra-jit")
        for r in rows:
            if r[0] == "skipped" and bucket(r[2]) != "no rustc symbol for function":
                why = r[2] if len(r[2]) < (10**6 if os.environ.get("RA_JIT_WHY") else 400) else r[2][:100] + " ... " + r[2][-300:]
                print(f"  skipped {r[1]}: {why}")
    print("MATCH" if ok else "MISMATCH")
    if not ok:
        got = fx.behaviour(fx.binary)
        import difflib
        want, have = (x[1].decode(errors='replace').splitlines() for x in (expected, got))
        if len(want) + len(have) > 40:
            # Long output: only where the two part ways.
            diff = list(difflib.unified_diff(want, have, "rustc", "ra-jit", lineterm="", n=1))
            print(f"  rustc: status {expected[0]}, ra-jit: status {got[0]}")
            print("\n".join("  " + l[:300] for l in diff[:60]))
        else:
            print(f"  rustc: status {expected[0]}\n{expected[1].decode(errors='replace')}")
            print(f"  ra-jit: status {got[0]}\n{got[1].decode(errors='replace')}")
        stderr = got[2].decode(errors='replace')
        print(stderr if len(stderr) < 1500 else stderr[:700] + "\n  ...\n" + stderr[-700:])
        do_memcheck = True
        if do_bisect:
            result["culprits"] = isolate(fx, expected)
            for c in result["culprits"]:
                print(f"  MISCOMPILED {c['function']}: {c['symptom']}")
    if ok and mixed:
        # The boundary between rustc's code and ours: leave a random half of the functions to
        # rustc, so that values made on one side (closures, futures, vtables) are used on the other.
        import random
        compiled = sorted(r[1] for rows in fx.reports.values() for r in rows if r[0] == "compiled")
        for seed in range(mixed):
            deny = random.Random(seed).sample(compiled, len(compiled) // 2)
            fx.ra_jit_build(deny)
            if not fx.matches(expected):
                got = fx.behaviour(fx.binary)
                print(f"  MIXED MISMATCH (seed {seed}): {symptom(expected, got)}")
                deny = minimize(deny, lambda d: fx.ra_jit_build(d) or not fx.matches(expected))
                fx.ra_jit_build(deny)
                print("    smallest set left to rustc that still differs:\n" + "\n".join("      " + d for d in deny))
                result["mixed_deny"] = deny
                result["match"] = False
                result["mixed_seed"] = seed
                do_memcheck = True
                break
        else:
            print(f"  mixed: {mixed} random splits match")
    if do_memcheck:
        errors = memcheck(fx)
        result["valgrind_errors"] = None if errors is None else len(errors)
        if errors:
            result["match"] = False
    if do_layouts:
        import layoutdiff
        compared, wrong = layoutdiff.diff(fx.ws, fx.crates, fx.layout_log)
        print(f"  layouts: {len(wrong)} of {compared} differ from rustc")
        for name, want, have in wrong:
            print(f"    {name}: rustc (size, align) {want}, rust-analyzer {have}")
        result["layouts"] = {"compared": compared, "wrong": [w[0] for w in wrong]}
        if wrong:
            result["match"] = False
    result["work"] = fx.work
    return result


def discard(results):
    """Scratch directories are only worth keeping for what did not match."""
    import shutil
    for r in results:
        if r.get("match"):
            shutil.rmtree(r["work"], ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workspace")
    ap.add_argument("--bin", required=True)
    ap.add_argument("--crate", action="append", required=True)
    ap.add_argument("--bisect", action="store_true")
    ap.add_argument("--memcheck", action="store_true", help="also fail on valgrind errors when the output matches")
    ap.add_argument("--layouts", action="store_true", help="compare type sizes and alignments with rustc's")
    ap.add_argument("--mixed", type=int, default=0, help="also run this many builds with a random half of the functions left to rustc")
    ap.add_argument("--json")
    a = ap.parse_args()

    fx = Fixture(a.workspace, a.crate, ["-p", a.bin])
    result = compare(fx, a.bin, a.bisect, a.memcheck, a.layouts, a.mixed)
    if a.json:
        json.dump(result, open(a.json, "w"), indent=1)
    discard([result])
    sys.exit(0 if result["match"] else 1)


if __name__ == "__main__":
    main()
