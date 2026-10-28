#!/usr/bin/env python3
"""How much of a build Relax's `merge_similar` would fold (issue #1292).

    merge_shapes.py SHAPES.tsv [--max-params 16]

SHAPES.tsv is what a build writes with `BLAZE_SHAPES` set and no store (`BLAZE_CACHE=off`), with
the `M` lines of `crates/hir-ty-codegen/masked.rs`: each function's Cranelift IR, as Cranelift is
given it, hashed with its integer and float immediates, the data it takes the address of and the
functions whose address it takes masked; its constants in order; its direct callees in order.

Replays makepad's `libs/relax/backend/src/merge.rs` (d5bed7b) on them: group by shape with the
callees exact, keep a group when Relax's profit test passes (with machine code bytes for its
estimate), give each member's callers the member's constants as extra arguments, so callers of
different members become alike, and repeat to a fixpoint. A member whose address is taken (a
`func_addr` in any function: vtables are data and not seen here, so this undercounts) becomes a
thunk. Prints functions, code bytes and compile time before and after, and the largest groups.
"""
import argparse
from collections import Counter, defaultdict


def load(path):
    masked, size, micros = {}, {}, {}
    for line in open(path, errors="replace"):
        tag, *rest = line.rstrip("\n").split("\t")
        if tag == "M":
            symbol, h, insts, consts, calls = rest
            masked[symbol] = (h, int(insts), consts.split("\x1f") if consts else [],
                              calls.split("\x1f") if calls else [])
        elif tag == "C":
            size[rest[0]] = int(rest[1])
        elif tag == "O":
            micros[rest[0]] = int(rest[6])
    return masked, size, micros


def sym(ref):
    """`F<symbol>+<offset>` as the symbol."""
    return ref[1:].rsplit("+", 1)[0] if ref.startswith("F") else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("shapes")
    ap.add_argument("--max-params", type=int, default=16)
    args = ap.parse_args()
    masked, size, micros = load(args.shapes)
    funcs = {s: dict(h=m[0], insts=m[1], consts=list(m[2]), calls=[sym(c) or c for c in m[3]])
             for s, m in masked.items()}
    total_n, total_b = len(funcs), sum(size.get(s, 0) for s in funcs)
    total_t = sum(micros.get(s, 0) for s in funcs)
    keep = {sym(c) for f in funcs.values() for c in f["consts"] if sym(c)}
    print(f"{total_n} functions with a masked shape, {total_b / 1e6:.2f} MB of code, "
          f"{total_t / 1e6:.2f} s compiling them; {len(keep & funcs.keys())} have their address taken")

    # A function's identity in its callers' shapes: its merged body once it has one.
    body = {s: s for s in funcs}
    # What a call of a member passes: the member's constants.
    passes = {}
    alive = set(funcs)
    thunks = set()
    removed_n = removed_b = removed_t = 0
    params = groups_total = 0
    report = []
    for rnd in range(1, 64):
        sites = Counter(c for s in alive for c in funcs[s]["calls"])
        by = defaultdict(list)
        for s in alive:
            if s in thunks:
                continue
            f = funcs[s]
            callees = tuple("SELF" if c == s else body.get(c, c) for c in f["calls"])
            # the constants the calls now pass, in the order of the calls
            consts = list(f["consts"])
            for c in f["calls"]:
                if c != s and c in passes:
                    consts.extend(passes[c])
            f["now"] = consts
            by[(f["h"], callees, len(consts))].append(s)
        made = 0
        for key, g in by.items():
            if len(g) < 2:
                continue
            g.sort()
            vals = [funcs[s]["now"] for s in g]
            pos = [i for i in range(len(vals[0])) if len({v[i] for v in vals}) > 1]
            if len(pos) > args.max_params:
                continue
            rep = g[0]
            est = size.get(rep, 0)
            n_sites = sum(sites.get(s, 0) for s in g)
            n_thunks = sum(1 for s in g if s in keep)
            saved = est * (len(g) - 1)
            cost = n_sites * len(pos) * 3 + n_thunks * (len(pos) * 3 + 16) + (est // 8 if pos else 0)
            if saved <= cost:
                continue
            made += 1
            groups_total += 1
            params += len(pos)
            mid = f"merged#{rnd}#{made}"
            funcs[mid] = dict(h=funcs[rep]["h"] + "m", insts=funcs[rep]["insts"],
                              consts=[], calls=[body.get(c, c) for c in funcs[rep]["calls"]])
            size[mid] = est
            alive.add(mid)
            for s in g:
                body[s] = mid
                passes[s] = [funcs[s]["now"][i] for i in pos]
                if s in keep:
                    thunks.add(s)
                else:
                    alive.discard(s)
                removed_n += 1
                removed_b += size.get(s, 0)
                removed_t += micros.get(s, 0)
            removed_n -= 1
            removed_b -= est
            removed_t -= micros.get(rep, 0)
            report.append((len(g), est, len(pos), rnd, rep))
        print(f"round {rnd}: {made} groups")
        if not made:
            break
    thunk_b = len(thunks) * 16
    print(f"\n{groups_total} groups, {params} extra parameters, {len(thunks)} thunks")
    print("| | functions | code | compile |\n|---|---:|---:|---:|")
    print(f"| today | {total_n} | {total_b / 1e6:.2f} MB | {total_t / 1e6:.2f} s |")
    print(f"| merged | {total_n - removed_n + len(thunks)} ({(total_n - removed_n + len(thunks)) / total_n:.0%}) "
          f"| {(total_b - removed_b + thunk_b) / 1e6:.2f} MB ({(total_b - removed_b + thunk_b) / total_b:.0%}) "
          f"| {(total_t - removed_t) / 1e6:.2f} s ({(total_t - removed_t) / max(total_t, 1):.0%}) |")
    print("\nLargest groups by bytes saved (members, bytes each, params, round, representative):")
    for n, est, p, rnd, rep in sorted(report, key=lambda r: -(r[0] - 1) * r[1])[:30]:
        print(f"  {n:5} {est:7} {p:3} {rnd:3}  {rep}")
    print("members per group:", sorted(Counter(r[0] for r in report).items())[:15])
    print("params per group:", sorted(Counter(r[2] for r in report).items()))


if __name__ == "__main__":
    main()
