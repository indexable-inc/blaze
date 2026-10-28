#!/usr/bin/env python3
"""Classify the census's build scripts: which blaze elides, and why the rest are not.

Reads /root/census/scripts/*/{build.rs,meta.json} and /root/census/projects.json; writes
/root/census/classes.json and prints a summary."""
import collections, json, os, re, subprocess, sys

W = "/root/census"
BIN = sys.argv[1] if len(sys.argv) > 1 else "/root/lane/target/release/blaze-build-script"
PROBES = {"autocfg", "rustc_version", "version_check", "rustversion"}

rows = {}
for key in sorted(os.listdir(f"{W}/scripts")):
    d = f"{W}/scripts/{key}"
    meta = json.load(open(f"{d}/meta.json"))
    src = open(f"{d}/build.rs", errors="replace").read()
    out = subprocess.run([BIN, "inert", f"{d}/build.rs", meta["edition"]], capture_output=True,
                         text=True)
    got = json.loads(out.stdout) if out.returncode == 0 else {"inert": False, "why": out.stderr}
    words = set(re.findall(r"cargo::?([a-z-]+)", src))
    deps = set(meta.get("build_deps", []))
    rows[key] = {
        "inert": got["inert"],
        "why": got.get("why"),
        "lines": src.count("\n"),
        "env": "env::var" in src or "env!" in src,
        "rerun": bool(words & {"rerun-if-changed", "rerun-if-env-changed"}),
        "check_cfg": "rustc-check-cfg" in words,
        "probe": sorted(deps & PROBES),
        "cc": bool(deps & {"cc", "cmake", "bindgen", "pkg-config", "vcpkg"}),
        "links": meta.get("links"),
    }
json.dump(rows, open(f"{W}/classes.json", "w"), indent=1)

projects = json.load(open(f"{W}/projects.json"))["projects"]
print(f"{'project':16} scripts inert  probe  native  other")
total = collections.Counter()
for name, keys in sorted(projects.items()):
    keys = [k for k in set(keys) if k in rows]
    c = collections.Counter()
    for k in keys:
        r = rows[k]
        c["scripts"] += 1
        c["inert" if r["inert"] else "probe" if r["probe"] else "native" if r["cc"] or r["links"]
          else "other"] += 1
    total.update(c)
    print(f"{name:16} {c['scripts']:7} {c['inert']:5} {c['probe']:6} {c['native']:7} {c['other']:6}")
print(f"{'(distinct)':16} {len(rows):7} {sum(r['inert'] for r in rows.values()):5}")

why = collections.Counter()
for k, r in rows.items():
    if not r["inert"] and not r["probe"] and not r["cc"] and not r["links"]:
        why[re.sub(r"\d+", "N", (r["why"] or "")[:70])] += 1
print("\nwhy the 'other' scripts are not inert:")
for w, n in why.most_common(40):
    print(f"{n:5}  {w}")
small = [k for k, r in rows.items() if not r["inert"] and r["lines"] < 25 and not r["probe"]
         and not r["cc"] and not r["links"]]
print(f"\nshort (<25 lines) 'other' scripts: {len(small)}")
for k in small[:60]:
    print("  ", k, "--", rows[k]["why"][:90])
