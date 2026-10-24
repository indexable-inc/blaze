#!/usr/bin/env python3
"""Scores the lints rust-analyzer finds, at the level the source sets, against rustc's UI tests.

usage: lints_ui.py <rust checkout> [--bin ra-jit] [--work dir] [--jobs N] [--filter substr]
                   [--json out.json] [--toolchain nightly-YYYY-MM-DD] [--list wrong|out]
                   [--deny LINT]

A lint a crate denies, with `#![deny(..)]`, `#[forbid(..)]` or `deny(warnings)`, fails rustc's
build, so ra-jit may only build what it is denied in if it fails the build too, and may only
fail one when rustc would. The tests are the `tests/ui` files that set a lint level with
`deny` or `forbid`; each revision is a unit, as `borrowck_ui.py` reads them.

What a unit should give is what the nightly rustc of the checkout's commit gives for the file,
as a library and with no lint flag on the command line. A unit is out of scope, with a counted
reason, when rustc reports an error that is no lint (it may stop before the lints), or when
`borrowck_ui.py` puts it out (a nightly feature, an auxiliary crate, another file, a flag that
is not inert), or when it sets a lint level on the command line.

Each lint is scored on its own, by the (lint, line) of every error: one ra-jit reports that
rustc does not is wrong, one rustc reports that ra-jit does not is missed. A lint whose
errors ra-jit may fail a build with must have none wrong. What ra-jit finds in a macro's
expansion is not counted: it fails no build.

`--deny LINT` scores one lint over every `tests/ui` file, not only those that set a level: rustc
warns of most lints by default, so any test is one for them. Each file is read with
`#![deny(LINT)]` put before its first line, which a level the file sets itself overrides, as it
would in a crate; a file that begins with a `#!` line that is no attribute is out of scope.
"""
import argparse, collections, concurrent.futures, hashlib, json, os, re, subprocess, sys, tempfile, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from borrowck_ui import DEFAULT_TOOLCHAIN, classify, ra_jit_run  # noqa: E402

LEVEL = re.compile(r"#!?\[(?:cfg_attr\([^\]]*)?\b(deny|forbid)\(")
LINT_FLAG = re.compile(r"^(-[AWDF]|--cap-lints|--force-warn)")
# rustc's lint names; rust-analyzer's own diagnostics are named with dashes, and `borrowck`
# is the borrow checker's errors that have no code.
LINT = re.compile(r"^(?!borrowck$)[a-z][a-z0-9_]*$")


# rustc names the crate after the file, so the prefix must keep it an identifier.
DENIED = "lints_ui_denied_"


def collect(root, every=False):
    ui = os.path.join(root, "tests", "ui")
    found = []
    for dirpath, dirnames, filenames in os.walk(ui):
        dirnames[:] = [d for d in dirnames if d != "auxiliary"]
        for name in filenames:
            if name.endswith(".rs") and not name.startswith(DENIED):
                path = os.path.join(dirpath, name)
                if every or LEVEL.search(open(path, errors="replace").read()):
                    found.append(path)
    return sorted(found)


def denied_copy(unit, lint, made):
    """Points the unit at a copy of its file that denies `lint`, beside it so that what the file
    names by a relative path is found; or says why it cannot."""
    text = open(unit.path, errors="replace").read()
    if text.startswith("#!") and not text.startswith("#!["):
        return "a `#!` line first"
    copy = os.path.join(os.path.dirname(unit.path), DENIED + os.path.basename(unit.path))
    if copy not in made:
        # On the first line, so that every line keeps its number.
        with open(copy, "w") as f:
            f.write(f"#![deny({lint})] " + text)
        made.add(copy)
    unit.path = copy
    return None


def rustc_errors(toolchain, unit, cache, work):
    """Each error rustc reports for the unit, (lint name or code or None, line); or why none."""
    text = open(unit.path, errors="replace").read()
    args = [unit.path, "--edition", unit.edition, "--error-format=json", "--emit=metadata", "--crate-type", "lib",
            "-Zui-testing"]
    # `-Zunpretty` prints the source and stops before any lint, so rustc checks it without.
    dropped = ("--crate-type", "--edition", "--error-format", "--emit", "-Zpolonius", "-Z polonius", "-Zunpretty",
               "-Z unpretty")
    for flag in unit.flags:
        if not flag.startswith(dropped):
            args += flag.split(" ", 1)
    key = hashlib.sha1(json.dumps([toolchain, args, text]).encode()).hexdigest()
    if key not in cache:
        with tempfile.TemporaryDirectory(dir=work) as out:
            try:
                # `-Zui-testing` and the tests' `-Z` flags on a stable toolchain too.
                env = dict(os.environ, RUSTC_BOOTSTRAP="1")
                done = subprocess.run(["rustc", f"+{toolchain}", *args, "--out-dir", out], capture_output=True,
                                      text=True, timeout=120, cwd=os.path.dirname(unit.path), env=env)
                cache[key] = [l for l in done.stderr.splitlines() if '"level":"error' in l]
            except subprocess.TimeoutExpired:
                cache[key] = "rustc timed out"
    if isinstance(cache[key], str):
        return cache[key]
    errors = []
    for line in cache[key]:
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("$message_type") != "diagnostic":
            continue
        if d.get("level") != "error":
            return "rustc crashed"
        if d["message"].startswith("aborting due to"):
            continue
        code = (d.get("code") or {}).get("code")
        # The one lint rustc writes with an error code: E0133 is the hard error too, which has no
        # lint level to note.
        if code == "E0133" and any(c["message"] == "the lint level is defined here" for c in d.get("children", [])):
            code = "unsafe_op_in_unsafe_fn"
        at = None
        for span in (s for s in d.get("spans", []) if s.get("is_primary")):
            while span and os.path.basename(span["file_name"]) != os.path.basename(unit.path):
                span = (span.get("expansion") or {}).get("span")
            if span:
                at = span["line_start"]
                break
        errors.append((code, at, d["message"]))
    return errors


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rust")
    ap.add_argument("--bin", default=os.environ.get("RA_JIT_BIN", "ra-jit"))
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "lints-ui"))
    ap.add_argument("--jobs", type=int, default=os.cpu_count())
    ap.add_argument("--filter")
    ap.add_argument("--json")
    ap.add_argument("--toolchain", default=DEFAULT_TOOLCHAIN)
    ap.add_argument("--list", choices=["wrong", "missed", "out"], action="append", default=[])
    ap.add_argument("--deny")
    args = ap.parse_args()
    # rustc runs in the directory of each file, so the paths it is given are absolute.
    args.rust = os.path.abspath(args.rust)
    os.makedirs(args.work, exist_ok=True)
    cache_path = os.path.join(args.work, "rustc-cache.json")
    cache = json.load(open(cache_path)) if os.path.exists(cache_path) else {}

    files = collect(args.rust, every=bool(args.deny))
    if args.filter:
        files = [f for f in files if args.filter in f]
    units = [u for f in files for u in classify(args.rust, f)]
    made = set()
    for unit in units:
        if unit.out and unit.out.startswith("compile flag -") and LINT_FLAG.match(unit.out.split()[-1]):
            unit.out = "a lint level on the command line"
        elif not unit.out and any(LINT_FLAG.match(f) for f in unit.flags):
            unit.out = "a lint level on the command line"
        elif not unit.out and args.deny:
            unit.out = denied_copy(unit, args.deny, made)
    try:
        score(args, units, files, cache, cache_path)
    finally:
        for copy in made:
            os.remove(copy)


def score(args, units, files, cache, cache_path):
    scored = (lambda code: code == args.deny) if args.deny else (lambda code: True)
    started = time.time()
    todo = [u for u in units if not u.out]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for unit, errs in zip(todo, pool.map(lambda u: rustc_errors(args.toolchain, u, cache, args.work), todo)):
            if isinstance(errs, str):
                unit.out = errs
            elif any(not (code and LINT.match(code)) for code, _, _ in errs):
                unit.out = "rustc reports an error that is no lint"
            else:
                unit.want = {(code, line) for code, line, _ in errs if scored(code)}
    json.dump(cache, open(cache_path, "w"))
    print(f"rustc: {len(todo)} runs in {time.time() - started:.0f}s", file=sys.stderr)

    inscope = [u for u in units if not u.out]
    started = time.time()
    actual = ra_jit_run(args.bin, inscope, "polonius", args.jobs, args.work)
    took = time.time() - started
    per = collections.defaultdict(lambda: {"right": 0, "wrong": 0, "missed": 0})
    wrong, missed, panics = [], [], []
    for unit in inscope:
        got = actual.get(unit.label, {})
        if "panic" in got:
            panics.append((unit.label, got["panic"].splitlines()[0][:120]))
            continue
        # A lint in what a macro expands to fails no build of ra-jit's (`broken_in`), so it is
        # not ra-jit's verdict to score.
        have = {(d["code"], d["line"]) for d in got.get("diagnostics", [])
                if d["severity"] == "Error" and LINT.match(d["code"]) and scored(d["code"])
                and not d.get("expanded")}
        messages = {(d["code"], d["line"]): d["message"] for d in got.get("diagnostics", [])}
        for it in have & unit.want:
            per[it[0]]["right"] += 1
        for it in have - unit.want:
            per[it[0]]["wrong"] += 1
            wrong.append((unit.label, it, messages.get(it, "")))
        for it in unit.want - have:
            per[it[0]]["missed"] += 1
            missed.append((unit.label, it))

    out = collections.Counter(u.out for u in units if u.out)
    print(f"{len(files)} files, {len(units)} units: {len(inscope)} in scope, {len(units) - len(inscope)} out; "
          f"ra-jit took {took:.0f}s, panicked on {len(panics)}")
    print(f"\n{'lint':40} {'right':>6} {'wrong':>6} {'missed':>7}")
    for lint, n in sorted(per.items(), key=lambda it: (-it[1]["right"] - it[1]["wrong"], it[0])):
        if n["right"] or n["wrong"]:
            print(f"{lint:40} {n['right']:6} {n['wrong']:6} {n['missed']:7}")
    others = sum(n["missed"] for n in per.values() if not (n["right"] or n["wrong"]))
    print(f"{'(lints rust-analyzer never reports)':40} {0:6} {0:6} {others:7}")
    if "out" in args.list:
        print("\nout of scope:")
        for why, n in out.most_common():
            print(f"  {n:6}  {why}")
    if "wrong" in args.list:
        print("\nwrong:")
        for label, (lint, line), message in wrong:
            print(f"  {label}:{line}  {lint}  {message[:100]}")
    if "missed" in args.list:
        print("\nmissed:")
        for label, (lint, line) in missed:
            if per[lint]["right"] or per[lint]["wrong"]:
                print(f"  {label}:{line}  {lint}")
    for label, why in panics:
        print(f"panic {label}: {why}")
    if args.json:
        json.dump({"toolchain": args.toolchain, "files": len(files), "units": len(units), "in_scope": len(inscope),
                   "lints": per, "wrong": wrong, "out": out}, open(args.json, "w"), indent=1, default=list)


if __name__ == "__main__":
    main()
