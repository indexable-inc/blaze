# Converts the published Ed25519 vectors (README.md says where each file comes from) into
# `published.txt`, the plain text `src/tests/published.rs` reads with `include_str!`; each
# expected reason is the one `edge.py`'s strict reference gives, step for step as the crate checks.
# Run by hand after changing a source file: python3 -I crates/blaze-ed25519/tests/data/convert.py
#
# Each line of published.txt: <source> <id> <label> <key> <message> <signature> <reason>, the
# bytes in lowercase hex ("-" for none), <label> the source's own verdict (speccheck has none:
# "case"), <reason> `Ok`, `Length` (a signature not of 64 bytes) or the crate's `Refused` variant
# (`KeyEncoding.YNotReduced` for one with an `Encoding`).

import collections, json, os, sys
sys.dont_write_bytecode = True
here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
import edge

def hexed(b): return b.hex() if b else "-"

lines = []
# "Taming the many EdDSAs" (Chalkias, Garillot, Nikolaenko, 2020), its 12 cases in its order.
paper = {0: "KeySmallOrder", 1: "KeySmallOrder", 2: "KeyMixedOrder", 3: "KeyMixedOrder",
         4: "KeyMixedOrder", 5: "KeyMixedOrder", 6: "SNotBelowOrder", 7: "SNotBelowOrder",
         8: "KeyMixedOrder", 9: "KeyMixedOrder", 10: "KeyEncoding.NegativeZero",
         11: "KeyEncoding.NegativeZero"}
cases = json.load(open(os.path.join(here, "speccheck-cases.json")))
assert len(cases) == 12
for i, case in enumerate(cases):
    key, msg, sig = (bytes.fromhex(case[f]) for f in ("pub_key", "message", "signature"))
    why = edge.verdict(key, msg, sig)
    assert why == paper[i], (i, why)
    lines.append("speccheck %d case %s %s %s %s" % (i, hexed(key), hexed(msg), hexed(sig), why))

wy = json.load(open(os.path.join(here, "wycheproof-ed25519_test.json")))
count = collections.Counter()
for group in wy["testGroups"]:
    key = bytes.fromhex(group["publicKey"]["pk"])
    for t in group["tests"]:
        msg, sig = bytes.fromhex(t["msg"]), bytes.fromhex(t["sig"])
        why = edge.verdict(key, msg, sig)
        label = t["result"]
        # blaze's policy against Wycheproof's labels: every valid accepted, every invalid
        # refused, every acceptable (a small-order or mixed-order key) refused for its key.
        if label == "valid": assert why == "Ok", t["tcId"]
        elif label == "invalid": assert why != "Ok", t["tcId"]
        else:
            assert label == "acceptable" and why in ("KeySmallOrder", "KeyMixedOrder"), (t["tcId"], why)
        count[(label, why)] += 1
        lines.append("wycheproof %d %s %s %s %s %s" % (t["tcId"], label, hexed(key), hexed(msg), hexed(sig), why))
assert len(lines) == 12 + wy["numberOfTests"]
open(os.path.join(here, "published.txt"), "w").write("\n".join(lines) + "\n")
for k, v in sorted(count.items()): print(k, v)
print(len(lines), "lines written")
