#!/usr/bin/env python3
"""frames.py: how much stack each function of a linked program takes, by build.

usage: frames.py LABEL=BINARY... [--base LABEL] [--top 10] [--json out.json] [--table out.md]

Each binary is disassembled (`objdump -d`, binutils') and each function's frame read from its
prologue: on x86-64 what it subtracts from `rsp` (or from `r11` or hands `__rust_probestack`, for
a frame probed page by page) plus 8 bytes per register it pushes; on AArch64 what it subtracts
from `sp`, with the pre-indexed stores that move it. A function whose frame is not set up in
its first instructions (one that only calls, a leaf that keeps everything in registers) counts
as 0.

Per build: how many functions, their frames' median, 90th and 99th percentile, the largest
(and whose), the sum, and how many take 4 KiB or more and 64 KiB or more. Against `--base`
(say rustc's `release`, kept as `bin-<label>` beside each program): over the functions both have, by name with the hash cut off, the sum of their frames
in each and how many are larger, and the `--top` functions whose frames grew the most.

Every objdump runs at once, each bounded; a failure names the binary, the command, its exit
code and its stderr's tail. Python 3.11+, stdlib only.
"""
import argparse
import asyncio
import json
import re
import statistics
import sys

# Instructions read for a prologue: past them a function has set up its frame.
PROLOGUE = 24
LIMIT_S = 600
HEADER = re.compile(r"^[0-9a-f]+ <(.+)>:$")
HASH = re.compile(r"::h[0-9a-f]{16}$|::\{shim:.*$")
IMM = r"\$?#?(0x[0-9a-f]+|\d+)"
X86_SUB = re.compile(rf"\bsub[lq]?\s+{IMM},%(rsp|r11)\b")
X86_LEA = re.compile(r"\blea\s+-(0x[0-9a-f]+|\d+)\(%rsp\),%r11\b")
X86_MOV = re.compile(rf"\bmov[lq]?\s+{IMM},%(eax|rax)\b")
X86_PUSH = re.compile(r"\bpush[q]?\s+%r")
A64_SUB = re.compile(r"\bsub\s+sp,\s*sp,\s*#(0x[0-9a-f]+|\d+)(?:,\s*lsl\s*#(\d+))?")
A64_PRE = re.compile(r"\b(?:stp|str)\s+[^\[]*\[sp,\s*#-(0x[0-9a-f]+|\d+)\]!")
STOP = re.compile(r"\b(ret|jmp|b|br|bl|blr|call[q]?)\b")


def number(text):
    return int(text, 16) if text.startswith("0x") else int(text)


def frame(lines):
    """The bytes the prologue in `lines` (instruction texts) sets aside."""
    pushed, frames, loaded = 0, [], None
    for text in lines[:PROLOGUE]:
        if m := X86_SUB.search(text):
            frames.append(number(m[1]))
        elif m := X86_LEA.search(text):
            frames.append(number(m[1]))
        elif m := X86_MOV.search(text):
            loaded = number(m[1])
        elif X86_PUSH.search(text):
            pushed += 8
        elif m := A64_SUB.search(text):
            frames.append(number(m[1]) << int(m[2] or 0))
        elif m := A64_PRE.search(text):
            pushed += number(m[1])
        elif "probestack" in text and loaded is not None:
            frames.append(loaded)
        elif STOP.search(text):
            break
    return pushed + max(frames, default=0)


async def disassemble(label, binary):
    argv = ["objdump", "-d", "-C", "--no-show-raw-insn", binary]
    proc = await asyncio.create_subprocess_exec(*argv, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(LIMIT_S):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        raise SystemExit(f"frames.py: {label}: `{' '.join(argv)}` took more than {LIMIT_S} s")
    if proc.returncode:
        raise SystemExit(f"frames.py: {label}: `{' '.join(argv)}` exited {proc.returncode}: "
                         f"{err.decode(errors='replace')[-2000:]}")
    functions, name, body = {}, None, []
    for line in out.decode(errors="replace").splitlines():
        if m := HEADER.match(line):
            if name is not None:
                functions[name] = max(functions.get(name, 0), frame(body))
            name, body = HASH.sub("", m[1]), []
        elif name is not None and "\t" in line:
            body.append(line.rsplit("\t", 1)[-1])
    if name is not None:
        functions[name] = max(functions.get(name, 0), frame(body))
    return label, functions


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))] if ordered else 0


def summary(functions):
    sizes = list(functions.values())
    largest = max(functions.items(), key=lambda it: it[1], default=("-", 0))
    return {"functions": len(sizes), "median": statistics.median(sizes) if sizes else 0,
            "p90": percentile(sizes, 0.9), "p99": percentile(sizes, 0.99), "max": largest[1],
            "max_fn": largest[0], "sum": sum(sizes), "ge_4k": sum(s >= 4096 for s in sizes),
            "ge_64k": sum(s >= 65536 for s in sizes)}


def against(base, other, top):
    common = base.keys() & other.keys()
    grew = sorted(((other[n] - base[n], n) for n in common), reverse=True)[:top]
    return {"common": len(common), "base_sum": sum(base[n] for n in common),
            "sum": sum(other[n] for n in common), "larger": sum(other[n] > base[n] for n in common),
            "smaller": sum(other[n] < base[n] for n in common),
            "grew": [{"fn": n, "base": base[n], "frame": other[n]} for d, n in grew if d > 0]}


def kib(n):
    return f"{n / 1024:.1f} KiB" if n >= 1024 else f"{n} B"


def table(results, base):
    rows = ["| Build | Functions | Median | p90 | p99 | Largest | Sum | >= 4 KiB | >= 64 KiB |",
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for label, it in results["builds"].items():
        s = it["summary"]
        rows.append(f"| {label} | {s['functions']} | {kib(s['median'])} | {kib(s['p90'])} | {kib(s['p99'])} | "
                    f"{kib(s['max'])} | {kib(s['sum'])} | {s['ge_4k']} | {s['ge_64k']} |")
    if base:
        rows += ["", f"Against {base}, over the functions both have (by name):", "",
                 "| Build | Common | Sum, base → build | Larger | Smaller |", "|---|---:|---:|---:|---:|"]
        for label, it in results["builds"].items():
            if c := it.get("against"):
                rows.append(f"| {label} | {c['common']} | {kib(c['base_sum'])} → {kib(c['sum'])} | "
                            f"{c['larger']} | {c['smaller']} |")
        for label, it in results["builds"].items():
            if (c := it.get("against")) and c["grew"]:
                rows += ["", f"Grew most in {label}:", "", "| Function | Base | Build |", "|---|---:|---:|"]
                rows += [f"| `{g['fn'][:120]}` | {kib(g['base'])} | {kib(g['frame'])} |" for g in c["grew"]]
    return "\n".join(rows) + "\n"


async def main():
    ap = argparse.ArgumentParser(description="How much stack each function of a linked program takes.")
    ap.add_argument("builds", nargs="+", metavar="LABEL=BINARY")
    ap.add_argument("--base")
    ap.add_argument("--top", type=int, default=10)
    ap.add_argument("--json")
    ap.add_argument("--table")
    a = ap.parse_args()
    pairs = [b.split("=", 1) for b in a.builds]
    if any(len(p) != 2 for p in pairs):
        ap.error("each build is LABEL=BINARY")
    async with asyncio.TaskGroup() as group:
        tasks = [group.create_task(disassemble(label, binary)) for label, binary in pairs]
    found = dict(t.result() for t in tasks)
    if a.base and a.base not in found:
        ap.error(f"--base {a.base} is not one of the builds")
    results = {"base": a.base, "builds": {}}
    for label, functions in found.items():
        entry = {"summary": summary(functions)}
        if a.base and label != a.base:
            entry["against"] = against(found[a.base], functions, a.top)
        results["builds"][label] = entry
    shown = table(results, a.base)
    sys.stdout.write(shown)
    if a.table:
        with open(a.table, "w") as out:
            out.write(shown)
    if a.json:
        with open(a.json, "w") as out:
            json.dump(results, out, indent=2)


if __name__ == "__main__":
    asyncio.run(main())
