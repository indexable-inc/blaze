#!/usr/bin/env python3
"""fnincr.py: what an edit inside one function of a big file costs, query by query.

usage: fnincr.py [--fns N] [--files K] [--rounds R] [--keep LOG]
       fnincr.py --workspace DIR --package P --crate C --file F --edit NAME OLD NEW [--edit ...]

Generates the workspace of `genbig.py` (N functions, default 500, in K files, default 1),
starts the daemon on it in watch mode with salsa's execution counts on (`RUSTD_TRACE`),
and makes each edit below R times, each followed by its undo. For every edit
it prints how often the queries that matter ran (lowering a body, inference, MIR, the place of
a panic), how many functions were compiled again, the time the daemon took to take the change
in, and the time to the binary. The undo is timed too but not printed. Medians over the rounds.
Comparing `--files 1` with `--files 50` tells what the size of the edited file costs.
With `--workspace`, the edits are of FILE of a workspace of one's own: each `--edit` replaces
the one OLD in it by NEW.
"""
import argparse, os, re, statistics, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
os.environ.update(BLAZE_STATS="1", RUSTD_TRACE="1")
sys.path.insert(0, HERE)
from difftest import Fixture, BLAZE, sysroot_rlibs
import genbig

# Short names for the queries that tell how much was redone, by the name salsa gives them.
QUERIES = {
    "lower": "Body::relative_",
    "map": "Body::with_source_map_",
    "body": "Body::of_",
    "infer": "InferenceResult<'db>::for_body_",
    "mir": "mir_body_query",
    "closure mir": "mir_body_for_closure_query",
    "expand": "MacroCallId::parse_macro_expansion_",
    # Before places.rs: `source_location` and `row_location` were the queries of places.
    "places": "source_place",
    "rows": "row_place",
    "anchors": "anchor_location",
}
COLUMNS = ["lower", "map", "body", "infer", "mir", "closure mir", "expand", "places", "rows", "anchors", "compiled",
           "apply ms", "build ms", "edit to binary ms", "cpu ms"]


def millis(duration):
    """Milliseconds in a duration as Rust's `Debug` prints it: `1.2s`, `35.1ms`, `80µs`."""
    value, unit = re.fullmatch(r"([\d.]+)(\D+)", duration).groups()
    return float(value) * {"s": 1000, "ms": 1, "µs": 1e-3, "us": 1e-3, "ns": 1e-6}[unit]


def cpu_ms(pid):
    """The processor time the process has used, all its threads, in ms."""
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    parts = [float(it) for it in out.replace("-", ":").split(":")]
    seconds = 0.0
    for part in parts:
        seconds = seconds * 60 + part
    return seconds * 1000


def edits(n):
    """Each edit by name: the function it is in (or `None` for the first file) and old, new."""
    mid, last = n // 2, n - 1
    out = {}
    for where, i in (("top", 0), ("middle", mid), ("bottom", last)):
        old = f"let edit_{i} = {i}u64;"
        out[f"literal in f{i} ({where})"] = (i, old, f"let edit_{i} = {i - i % 10 + (i + 1) % 10}u64;")
        out[f"line added in f{i} ({where})"] = (i, old, old + f"\n    let _more = edit_{i} + 1;")
    out["blank line at the top of the file"] = (0, "\n/// Function 0.\n", "\n\n/// Function 0.\n")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fns", type=int, default=500)
    ap.add_argument("--files", type=int, default=1)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--keep")
    ap.add_argument("--only", help="the edits whose name contains this")
    ap.add_argument("--profile", help="samples the daemon during the edits into this file (macOS `sample`)")
    ap.add_argument("--workspace")
    ap.add_argument("--package")
    ap.add_argument("--crate")
    ap.add_argument("--file")
    ap.add_argument("--edit", nargs=3, action="append", metavar=("NAME", "OLD", "NEW"))
    a = ap.parse_args()
    if a.workspace:
        ws, package, crate = os.path.abspath(a.workspace), a.package, a.crate
        chosen_edits = {name: (None, old, new) for name, old, new in a.edit}
        file_of = lambda _: os.path.join(ws, a.file)
    else:
        ws, package, crate = tempfile.mkdtemp(prefix="blaze-fnincr-"), "big", "big"
        sys.argv = ["genbig.py", ws, str(a.fns), str(a.files)]
        genbig.main()
        chosen_edits = edits(a.fns)

        def file_of(i):
            return os.path.join(ws, "src/big.rs" if a.files == 1 else f"src/big/m{i * a.files // a.fns}.rs")

    fx = Fixture(ws, [crate], ["-p", package])
    fx.rustc_build()
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    log_path = os.path.join(fx.work, "daemon.log")
    log_out, log = open(log_path, "a"), open(log_path)
    obj = os.path.join(fx.work, "blaze.o")
    daemon = subprocess.Popen([BLAZE, ws, "--crate", crate, "--out", obj, "--watch", *symbols],
                              stderr=log_out, stdout=log_out)
    done = 0

    def wait_build():
        nonlocal done
        while True:
            log.seek(0)
            text = log.read()
            found = len(re.findall(r"edit-to-binary: \S+", text))
            # The counts are printed after the binary is written.
            traced = text[text.rfind("change: "):].count("\norder: ") > 0
            if found > done and traced and found == len(re.findall(r"change: \d+ files", text)):
                done = found
                # The lines of the build that just finished.
                return text[text.rfind("change: "):]
            if daemon.poll() is not None:
                raise SystemExit(f"daemon died:\n{text[-3000:]}")
            time.sleep(0.05)

    def write(path, text):
        time.sleep(0.3)
        open(path, "w").write(text)
        return wait_build()

    results, originals = {}, {}
    try:
        while "watching" not in open(log_path).read():
            if daemon.poll() is not None:
                raise SystemExit(f"daemon died:\n{open(log_path).read()[-3000:]}")
            time.sleep(0.05)
        chosen = {k: v for k, v in chosen_edits.items() if not a.only or a.only in k}
        sampler = a.profile and subprocess.Popen(
            ["sample", str(daemon.pid), "3600", "1", "-file", a.profile], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for name, (i, old, new) in chosen.items():
            path = file_of(i)
            original = originals.setdefault(path, open(path).read())
            assert original.count(old) == 1, (path, old)
            for _ in range(a.rounds):
                time.sleep(0.3)
                before = cpu_ms(daemon.pid)
                build = write(path, original.replace(old, new))
                spent = cpu_ms(daemon.pid) - before
                m = re.search(r"^ran: (.*)$", build, re.M)
                # Some names have spaces in them: `InferenceResult < 'db >::for_body_`.
                ran = {k.replace(" ", ""): v for k, v in re.findall(r"(.+?)=(\d+)(?: |$)", m[1] if m else "")}
                row = {short: int(ran.get(query, 0)) for short, query in QUERIES.items()}
                row["places"] += int(ran.get("source_location", 0))
                row["rows"] += int(ran.get("row_location", 0))
                row["cpu ms"] = spent
                m = re.search(r"stats: (\d+) of \d+ functions compiled", build)
                row["compiled"] = int(m[1]) if m else -1
                m = re.search(r"edit \(apply (\S+)\): .* in (\S+) ", build)
                row["apply ms"], row["build ms"] = (millis(m[1]), millis(m[2])) if m else (0, 0)
                m = re.search(r"edit-to-binary: (\S+)", build)
                row["edit to binary ms"] = millis(m[1]) if m else 0
                results.setdefault(name, []).append(row)
                write(path, original)
            med = {k: statistics.median(r[k] for r in results[name]) for k in COLUMNS}
            print(name, {k: round(v, 1) for k, v in med.items()}, flush=True)
        if sampler:
            sampler.send_signal(2)
            sampler.wait()
    finally:
        daemon.kill()
        for path, text in originals.items():
            open(path, "w").write(text)
        if a.keep:
            open(a.keep, "w").write(open(log_path).read())
    what = a.file if a.workspace else f"{a.fns} functions in {a.files} file(s)"
    print(f"\n{what}, median of {a.rounds}\n")
    print("| Edit | " + " | ".join(COLUMNS) + " |\n|" + "---|" * (len(COLUMNS) + 1))
    for name, rows in results.items():
        med = [statistics.median(r[k] for r in rows) for k in COLUMNS]
        print(f"| {name} | " + " | ".join(f"{v:.0f}" for v in med) + " |")


if __name__ == "__main__":
    main()
