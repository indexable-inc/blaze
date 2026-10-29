#!/usr/bin/env python3
"""frames.py OBJ...: each function's stack frame, from the prologue of its aarch64 or x86-64 code.

Prints JSON: {"functions": n, "total": bytes, "largest": [[bytes, symbol], ...], "of": {symbol: bytes}}
for the symbols named in FRAMES_OF (comma-separated substrings)."""
import json, os, re, subprocess, sys

FUNC = re.compile(r"^[0-9a-f]+ <(.+)>:$")
SUB_IMM = re.compile(r"\bsub\s+sp, sp, #(0x[0-9a-f]+|\d+)(?:, lsl #12)?")
MOV16 = re.compile(r"\bmov[zk]?\s+x16, #(0x[0-9a-f]+|\d+)(?:, lsl #(\d+))?")
SUB_X16 = re.compile(r"\bsub\s+sp, sp, x16")
X86 = re.compile(r"\bsub\s+\$0x([0-9a-f]+),%rsp")


def frames(path):
    out = subprocess.run(["objdump", "-d", "--no-show-raw-insn", path], capture_output=True, text=True, check=True).stdout
    sizes, name, seen, x16 = {}, None, 0, 0
    for line in out.splitlines():
        if m := FUNC.match(line):
            name, seen, x16 = m.group(1), 0, 0
            sizes[name] = 0
            continue
        if name is None or seen > 12:
            continue
        seen += 1
        if m := SUB_IMM.search(line):
            value = int(m.group(1), 0) << (12 if "lsl #12" in line else 0)
            sizes[name] += value
        elif m := MOV16.search(line):
            value = int(m.group(1), 0) << int(m.group(2) or 0)
            x16 = value if "movk" not in line else x16 | value
        elif SUB_X16.search(line):
            sizes[name] += x16
        elif m := X86.search(line):
            sizes[name] += int(m.group(1), 16)
    return sizes


sizes = {}
for path in sys.argv[1:]:
    sizes.update(frames(path))
wanted = [it for it in os.environ.get("FRAMES_OF", "").split(",") if it]
print(json.dumps({
    "functions": len(sizes),
    "total": sum(sizes.values()),
    "over_4k": sum(1 for it in sizes.values() if it > 4096),
    "largest": sorted(((v, k) for k, v in sizes.items()), reverse=True)[:10],
    "of": {k: v for k, v in sizes.items() if any(w in k for w in wanted)},
}, indent=1))
