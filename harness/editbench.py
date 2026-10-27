#!/usr/bin/env python3
"""Edit-loop latency: ra-jit daemon versus cargo, on the same one-line edits.

usage: editbench.py [name ...] [--edits N]

Benchmarks are listed in benches.json. For each one the daemon is started in watch mode, a
literal in a function body is flipped back and forth N times, and the daemon's own timings are
collected, and `save_ms`, from the file written to the daemon saying it linked, which has the
daemon's notice of the write in it. Then the same edits are timed against cargo. Results go to bench.json and BENCH.md.

RA_JIT_SPLICE: edits are added to the linked binary where that can be done, and the results go
to bench-splice.json and BENCH-splice.md instead.

The `regex` bench is of bench2, which builds the regex-automata and regex-syntax that
`vendor.sh regex-automata-<version>` and `vendor.sh regex-syntax-<version>` unpack.
"""
import json, os, re, statistics, subprocess, sys, time

# ra-jit computes layouts with rust-analyzer's copy of rustc_abi, so everything it links against
# has to come from the rustc that copy was taken from.
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolchain")).read().strip())
import linkline
from difftest import Fixture, RA_JIT, WINDOWS, sysroot_rlibs

HERE = os.path.dirname(os.path.abspath(__file__))
SPLICE = bool(os.environ.get("RA_JIT_SPLICE"))
UNITS = {"ns": 1e-6, "µs": 1e-3, "ms": 1, "s": 1000}


def millis(text):
    value, unit = re.fullmatch(r"([0-9.]+)(ns|µs|ms|s)", text).groups()
    return float(value) * UNITS[unit]


class Edit:
    def __init__(self, path, find, replace):
        self.path, self.find, self.replace = path, find, replace
        self.original = open(path).read()
        assert find in self.original, f"{find!r} not in {path}"
        self.flipped = False

    def flip(self):
        self.flipped = not self.flipped
        text = self.original.replace(self.find, self.replace, 1) if self.flipped else self.original
        open(self.path, "w").write(text)

    def restore(self):
        open(self.path, "w").write(self.original)


def resident_mb(pid):
    """The process's resident set, as the platform reports it."""
    if WINDOWS:
        import ctypes, ctypes.wintypes as wt

        class Counters(ctypes.Structure):
            _fields_ = [("cb", wt.DWORD), ("PageFaultCount", wt.DWORD)] + [
                (name, ctypes.c_size_t) for name in (
                    "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage", "QuotaPagedPoolUsage",
                    "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage")]

        handle = ctypes.windll.kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        counters = Counters(cb=ctypes.sizeof(Counters))
        ctypes.windll.psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb)
        ctypes.windll.kernel32.CloseHandle(handle)
        return counters.WorkingSetSize // (1024 * 1024)
    return int(re.search(r"VmRSS:\s+(\d+)", open(f"/proc/{pid}/status").read())[1]) // 1024


def ra_jit_loop(fx, crate, edit, n):
    obj, patch, binary = (os.path.join(fx.work, f"ra-jit.{ext}") for ext in ("o", "patch.o", "bin"))
    args = linkline.with_output(linkline.ahead(fx.link_args, [patch, obj] if SPLICE else [obj], RA_JIT), binary)
    splice = ["--patch", patch, "--splice", binary] if SPLICE else []
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    # The daemon and the linker it runs share this handle's file position, so read through another one.
    log_path = os.path.join(fx.work, "daemon.log")
    log = open(log_path, "a", encoding="utf-8")
    daemon = subprocess.Popen(
        [RA_JIT, fx.ws, "--crate", crate, "--out", obj, "--watch", "--link-cmd", linkline.join(args), *splice, *symbols],
        stderr=log, stdout=log)

    def read_log():
        return open(log_path, encoding="utf-8", errors="replace").read()

    def wait_for(pattern, seen):
        deadline = time.time() + 300
        while time.time() < deadline:
            found = re.findall(pattern, read_log())
            if len(found) > seen:
                return found
            if daemon.poll() is not None:
                raise SystemExit(f"daemon died:\n{read_log()[-2000:]}")
            time.sleep(0.002)
        raise SystemExit("daemon timed out")

    try:
        wait_for(r"watching \d+ files", 0)
        text = read_log()
        cold = {"load_ms": millis(re.search(r"load workspace: (\S+)", text)[1]),
                "codegen_ms": millis(re.search(r"cold: compiled .* in (\S+) \(", text)[1])}
        coverage = re.search(r"cold: compiled (\d+) of (\d+)", text)
        saves = []
        for i in range(n):
            time.sleep(0.05)
            saved = time.monotonic()
            edit.flip()
            wait_for(r"edit-to-binary: \S+", i)
            saves.append((time.monotonic() - saved) * 1000)
        text = read_log()
        rss = resident_mb(daemon.pid)
    finally:
        daemon.kill()
        edit.restore()
    series = {
        "apply": [millis(m) for m in re.findall(r"edit \(apply (\S+)\)", text)],
        "codegen": [millis(m) for m in re.findall(r"edit \(apply \S+\): compiled .* in (\S+) \(", text)],
        "link": [millis(m) for m in re.findall(r"^link: ([0-9.]+(?:ns|µs|ms|s)) \(", text, re.M)][1:],
        "total": [millis(m) for m in re.findall(r"edit-to-binary: (\S+)", text)],
    }
    return {"cold": cold, "rss_mb": rss, "compiled": int(coverage[1]), "functions": int(coverage[2]),
            **{f"{k}_ms": statistics.median(v) for k, v in series.items()},
            "save_ms": statistics.median(saves), "first_edit_ms": series["total"][0], "spliced": len(re.findall(r"^link: .*\(spliced", text, re.M)),
            "edits": n}


def cargo_loop(fx, edit, n, cargo_args):
    times = []
    try:
        for _ in range(n):
            edit.flip()
            start = time.time()
            subprocess.run(["cargo", *cargo_args], cwd=fx.ws, capture_output=True, check=True)
            times.append((time.time() - start) * 1000)
    finally:
        edit.restore()
    return statistics.median(times)


def main():
    names = [a for a in sys.argv[1:] if not a.startswith("--")]
    n = int(sys.argv[sys.argv.index("--edits") + 1]) if "--edits" in sys.argv else 10
    benches = json.load(open(os.path.join(HERE, "benches.json")))
    suffix = "-splice" if SPLICE else ""
    out_path = os.path.join(HERE, f"bench{suffix}.json")
    results = json.load(open(out_path)) if os.path.exists(out_path) else {}
    for name, b in benches.items():
        if names and name not in names:
            continue
        ws = os.path.join(HERE, b["workspace"])
        touch = os.path.join(ws, b["touch"]) if "touch" in b else None
        fx = Fixture(ws, [b["crate"]], b["target"], touch=touch)
        fx.rustc_build()
        edit = Edit(os.path.join(ws, b["file"]), b["find"], b["replace"])
        result = ra_jit_loop(fx, b["crate"], edit, n)
        result["cargo_ms"] = cargo_loop(fx, edit, n, b["cargo"])
        results[name] = result
        print(name, json.dumps(result))
    json.dump(results, open(out_path, "w"), indent=1, sort_keys=True)
    how = " with `RA_JIT_SPLICE`" if SPLICE else ""
    lines = ["# ra-jit edit-loop bench", "", f"Generated by `harness/editbench.py`{how}. Medians over the edits; times in ms.", "",
             "| Bench | Fast-tier functions | Apply edit | Re-analysis + codegen | Link | ra-jit total | cargo | Daemon RSS | Cold load + codegen |",
             "|---|---|---|---|---|---|---|---|---|"]
    for name, r in results.items():
        spliced = f" ({r['spliced']}/{r['edits']} spliced)" if "spliced" in r else ""
        lines.append(
            f"| {name} | {r['compiled']} / {r['functions']} | {r['apply_ms']:.2f} | {r['codegen_ms']:.1f} | "
            f"{r['link_ms']:.0f}{spliced} | {r['total_ms']:.0f} | {r['cargo_ms']:.0f} | {r['rss_mb']} MB | "
            f"{r['cold']['load_ms'] + r['cold']['codegen_ms']:.0f} |")
    open(os.path.join(HERE, f"BENCH{suffix}.md"), "w").write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
