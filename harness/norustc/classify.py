#!/usr/bin/env python3
"""Counts the rustc, rustdoc and cargo runs count.py logged, per step and per kind.

    classify.py NAME [--json | --summary]

A run belongs to the step whose window (began..ended) holds its time. Its kind says what it
was for and who started it, read from its arguments and its parents' command lines.
"""
import json, os, sys
from collections import Counter, defaultdict

L = os.path.expanduser(os.environ.get("NR_ROOT", "~/no-rustc"))


def rows(name):
    for line in open(f"{L}/logs/{name}.log", errors="replace"):
        parts = line.rstrip("\n").split("\t")
        if len(parts) != 9:
            continue
        ts, tool, pid, ppid, pc, gpid, gpc, cwd, args = parts
        yield dict(ts=float(ts), tool=tool, pid=pid, ppid=ppid, pc=pc, gpid=gpid, gpc=gpc, cwd=cwd,
                   args=[a for a in args.split("\x1f") if a])


def value(args, flag):
    for i, a in enumerate(args):
        if a == flag and i + 1 < len(args):
            return args[i + 1]
        if a.startswith(flag + "="):
            return a[len(flag) + 1:]
    return None


def subcommand(args):
    for a in args:
        if not a.startswith(("-", "+")):
            return a
    return "?"


def who(pc):
    """Who a parent's command line says it is."""
    first = pc.split(" ")[0]
    if "rustc-wrapper" in pc:
        return "wrapper"
    if first.endswith("/rustdoc") or first.endswith("/rustdoc.real"):
        return "rustdoc"
    if first.endswith("/blaze-engine") and " --test " in pc and "--crate-name" in pc:
        return "blaze as rustdoc"
    if "build-script" in first or "/build/" in first:
        return "build script"
    if " serve" in pc and (first.endswith(("/blaze", "/blaze-engine")) or first in ("blaze", "blaze-engine")):
        return "worker"
    if first.endswith("/blaze") or first == "blaze":
        return "client"
    if "/blaze-engine " in pc or pc.startswith("blaze-engine"):
        return "blaze"
    if "cargo" in pc.split(" ")[0]:
        return "cargo"
    if "count.py" in pc or "taskset" in pc:
        return "user"
    return pc.split(" ")[0].rsplit("/", 1)[-1] or "?"


def kind(r, cargos):
    a, pc = r["args"], r["pc"]
    if r["tool"] == "cargo":
        sub = subcommand(a)
        starter = who(pc)
        if sub == "blaze":
            return "blaze (typed)"
        extra = ""
        for flag in ("--unit-graph", "--compile-time-deps", "--no-deps", "--doc"):
            if flag in a:
                extra += " " + flag
        return f"cargo {sub}{extra} (started by {starter})"
    if r["tool"] == "rustdoc":
        return f"rustdoc ({'test' if '--test' in a else 'doc'}, started by {who(pc)})"
    # rustc
    name = value(a, "--crate-name") or ""
    emit = value(a, "--emit") or ""
    ctype = ",".join(v for i, v in enumerate(a) if i and a[i - 1] == "--crate-type") or value(a, "--crate-type") or ""
    parent = who(pc)
    via = ""
    if parent == "cargo" or parent == "wrapper":
        cpid = r["ppid"] if parent == "cargo" else r["gpid"]
        c = cargos.get(cpid)
        if c:
            flags = "".join(" " + f for f in ("--unit-graph", "--compile-time-deps", "--doc") if f in c["args"])
            via = f" under `cargo {subcommand(c['args'])}{flags}` from {who(c['pc'])}"
    if parent == "rustdoc":
        return "rustc compiles doctest examples (rustdoc's)"
    if parent == "blaze as rustdoc" and "-" in a:
        if "--emit=metadata" in a:
            return "rustc checks a doctest example (blaze as rustdoc)"
        return "rustc links a doctest probe or example (blaze as rustdoc)"
    if "-vV" in a or "--version" in a or "-V" in a or (any(x.startswith("--print") for x in a) and not name):
        prints = sorted({(value([x], "--print") or (a[a.index(x) + 1] if x == "--print" and a.index(x) + 1 < len(a) else "")).split("=")[0]
                         for x in a if x.startswith("--print")})
        q = "-vV" if ("-vV" in a or "--version" in a or "-V" in a) else "--print " + ",".join(p for p in prints if p)
        return f"rustc query {q} (from {parent}{via})"
    if name == "___":
        return f"rustc query target info (from {parent}{via})"
    if name == "blaze_asm":
        return "rustc assembles asm!"
    if parent == "build script":
        return "rustc probe by a build script"
    if "-" in a and parent in ("blaze", "client", "worker") or (parent == "blaze" and "--crate-type=bin" in a):
        return "rustc compiles a doctest example" + (" (metadata only)" if "--emit=metadata" in a else "")
    if name.startswith("build_script_"):
        return f"rustc compiles a build script{via}"
    if "proc-macro" in ctype:
        return f"rustc compiles a proc macro{via}"
    src = next((x for x in a if x.endswith(".rs")), "")
    if src.endswith(".blaze.rs"):
        return f"rustc builds a stand-in{via}"
    if emit == "dep-info":
        return f"rustc writes dep-info for a daemon program{via}"
    linked = "bin" in ctype or "--test" in a
    what = "a test executable" if "--test" in a else "a program" if "bin" in ctype else "a library"
    check = "link" not in emit.split(",")
    verb = "checks" if check else "links" if linked else "compiles"
    ws = "workspace" if parent == "wrapper" else "dependency"
    return f"rustc {verb} {what} ({ws}){via}"


def main():
    name = sys.argv[1]
    steps = [json.loads(l) for l in open(f"{L}/logs/{name}.steps.jsonl")]
    all_rows = list(rows(name))
    cargos = {r["pid"]: r for r in all_rows if r["tool"] == "cargo"}
    # A cargo started with `$CARGO` (cargo's own path, set for what it runs) is not logged
    # itself; it is seen through the rustc runs it starts.
    for r in list(all_rows):
        if r["tool"] == "rustc" and r["pc"].split(" ")[0].endswith("/cargo.real") and r["ppid"] not in cargos:
            seen = dict(ts=r["ts"], tool="cargo", pid=r["ppid"], ppid=r["gpid"], pc=r["gpc"], gpid="", gpc="", cwd="",
                        args=r["pc"].split()[1:])
            cargos[r["ppid"]] = seen
            all_rows.append(seen)
    out = {}
    for s in steps:
        inside = [r for r in all_rows if s["began"] <= r["ts"] <= s["ended"]]
        counts = Counter(kind(r, cargos) for r in inside)
        tools = Counter(r["tool"] for r in inside)
        out[s["step"]] = dict(code=s["code"], secs=s["secs"], tools=dict(tools), kinds=dict(counts.most_common()),
                              notes=s["notes"])
    outside = [r for r in all_rows if not any(s["began"] <= r["ts"] <= s["ended"] for s in steps)]
    if outside:
        out["(between steps)"] = dict(tools=dict(Counter(r["tool"] for r in outside)),
                                      kinds=dict(Counter(kind(r, cargos) for r in outside).most_common()))
    if "--summary" in sys.argv:
        for step, s in out.items():
            queries = sum(n for k, n in s["kinds"].items() if k.startswith("rustc query"))
            rustc = s["tools"].get("rustc", 0)
            cargos = sum(n for k, n in s["kinds"].items() if k.startswith("cargo ") and "typed" not in k)
            print(f"{step:12s} {str(s.get('secs')):>6s}s  rustc {rustc:4d} = {rustc - queries:4d} compiles + "
                  f"{queries:3d} queries; rustdoc {s['tools'].get('rustdoc', 0)}; cargo started {cargos}")
        return
    if "--json" in sys.argv:
        print(json.dumps(out, indent=1))
        return
    for step, s in out.items():
        print(f"== {step}: exit {s.get('code')} {s.get('secs')} s  {s['tools']}")
        for k, n in s["kinds"].items():
            print(f"   {n:5d}  {k}")
        for note in s.get("notes", [])[-6:]:
            print(f"         | {note[:200]}")


if __name__ == "__main__":
    main()
