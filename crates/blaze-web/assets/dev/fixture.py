"""Synthetic build-page fixtures shaped like a real program (docs/web.md, "The snapshot").

    python3 fixture.py '{"nodes": 80000, "edges": 300000, "seed": 1, "out": "anim-like.blzw"}'

writes the snapshot `out` and, beside it, `<stem>.edit.cbor`: the records of one build of an
edit (Began, Started/Ended, a Functions of the functions compiled again, their Rebuilt, Finished) as a
CBOR sequence (RFC 8742) of blaze_protocol::events::Record, each item what one websocket
message carries, for `index.html?fixture=<out>&records=<stem>.edit.cbor`. CBOR rather than
JSON because a `key` is a u64, which JSON.parse rounds.
"""

from __future__ import annotations

import dataclasses
import json
import math
import random
import struct
import sys
from pathlib import Path
from typing import Any

MAGIC = b"BLZW"
VERSION = 1
HOW_COMPILED, HOW_UNIT, HOW_UNCHANGED, HOW_STORED = 0, 1, 2, 3


@dataclasses.dataclass(frozen=True)
class Input:
    nodes: int
    edges: int
    seed: int
    out: str
    edited: int = 3
    recompiled: int = 30
    build: int = 7
    root: str = "/home/dev/anim"


def parse_input(raw: str) -> Input:
    text = sys.stdin.read() if raw == "-" else Path(raw).read_text() if raw.endswith(".json") else raw
    value = json.loads(text)
    if not isinstance(value, dict):
        raise SystemExit("fixture.py: the input is not a JSON object")
    fields = {f.name: f for f in dataclasses.fields(Input)}
    unknown = sorted(set(value) - set(fields))
    if unknown:
        raise SystemExit(f"fixture.py: unknown fields {unknown}")
    for name, f in fields.items():
        if name not in value:
            if f.default is dataclasses.MISSING:
                raise SystemExit(f"fixture.py: field {name!r} is missing")
            continue
        want = int if f.type == "int" else str
        got = value[name]
        if type(got) is not want:
            raise SystemExit(f"fixture.py: field {name!r} is {type(got).__name__}, not {want.__name__}")
    inp = Input(**value)
    if inp.nodes < 200:
        raise SystemExit("fixture.py: field 'nodes' must be at least 200")
    if inp.edges < 0:
        raise SystemExit("fixture.py: field 'edges' must not be negative")
    if not 1 <= inp.edited <= inp.recompiled < inp.nodes // 10:
        raise SystemExit("fixture.py: fields 'edited' and 'recompiled' need 1 <= edited <= recompiled < nodes/10")
    return inp


# (crate, share of the functions). The app crate is the biggest; std, core and alloc are large;
# the dependencies fall off like a real lock file's.
CRATES = [
    ("anim", 0.30), ("core", 0.14), ("alloc", 0.07), ("std", 0.08),
    ("wgpu_core", 0.05), ("wgpu_hal", 0.03), ("naga", 0.04), ("winit", 0.025),
    ("image", 0.02), ("png", 0.012), ("glam", 0.02), ("serde", 0.015), ("serde_json", 0.01),
    ("hashbrown", 0.012), ("rayon_core", 0.01), ("rayon", 0.008), ("crossbeam_epoch", 0.006),
    ("crossbeam_deque", 0.005), ("smallvec", 0.006), ("arrayvec", 0.004), ("log", 0.003),
    ("bytemuck", 0.004), ("parking_lot", 0.005), ("parking_lot_core", 0.004), ("libc", 0.006),
    ("memchr", 0.004), ("fdeflate", 0.003), ("miniz_oxide", 0.004), ("flate2", 0.003),
    ("ahash", 0.003), ("bitflags", 0.002), ("thiserror", 0.001), ("anim_macros", 0.002),
    ("anim_scene", 0.03), ("anim_render", 0.035), ("anim_assets", 0.02), ("tracing", 0.006),
    ("tracing_core", 0.004), ("once_cell", 0.002), ("indexmap", 0.006),
]
SYSTEM = ("core", "alloc", "std")
# How deep in the dependency graph each crate is: calls between crates go down it, but for
# the few of generic code instantiated for a caller.
RANK = {
    "anim": 0, "anim_scene": 1, "anim_render": 1, "anim_assets": 1, "anim_macros": 1,
    "wgpu_core": 2, "winit": 2, "image": 2, "rayon": 2, "wgpu_hal": 3, "naga": 3, "png": 3, "glam": 3,
    "serde_json": 3, "rayon_core": 3, "flate2": 3, "serde": 4, "crossbeam_deque": 4, "parking_lot": 4,
    "fdeflate": 4, "miniz_oxide": 4, "tracing": 4, "indexmap": 4, "hashbrown": 5, "crossbeam_epoch": 5,
    "smallvec": 5, "arrayvec": 5, "bytemuck": 5, "parking_lot_core": 5, "tracing_core": 5, "log": 6,
    "memchr": 6, "ahash": 6, "bitflags": 6, "thiserror": 6, "once_cell": 6, "libc": 7,
    "std": 8, "alloc": 9, "core": 10,
}
WORDS = (
    "scene render mesh frame buffer queue device surface texture shader pipeline layout bind "
    "group pass encoder command adapter instance window event loop input key mouse camera light "
    "material vertex index uniform storage sampler view target swap present resize config error "
    "parse read write load store cache pool alloc map table node edge graph tree path walk visit "
    "build emit lower check resolve infer span token lexer item expr stmt block func module sync "
    "lock mutex condvar thread task spawn join wake poll future stream iter slice vec string fmt "
    "debug display hash eq ord clone drop default from into try as ptr mem raw cell rc arc box"
).split()
VERBS = "new get set with push pop insert remove len is_empty clear update draw step tick init run apply map fold next flush finish begin end open close".split()
TYPES = "Frame Scene Mesh Buffer Device Queue Texture Shader Pipeline Encoder Window Event Camera Light Vertex Node Graph Cache Pool Table Entry Error Config State Handle Context Builder Writer Reader Iter".split()
TRAITS = [
    "core::fmt::Debug>::fmt", "core::fmt::Display>::fmt", "core::clone::Clone>::clone",
    "core::ops::drop::Drop>::drop", "core::iter::traits::iterator::Iterator>::next",
    "core::cmp::PartialEq>::eq", "core::hash::Hash>::hash", "core::default::Default>::default",
    "core::convert::From<u32>>::from", "serde::ser::Serialize>::serialize",
]


def modules_of(rng: random.Random, krate: str, count: int) -> list[str]:
    mods = max(1, int(math.sqrt(count) / 2.5))
    out = [krate]
    for _ in range(mods - 1):
        depth = rng.choice((1, 1, 2, 2, 3))
        segs = [rng.choice(WORDS) for _ in range(depth)]
        out.append("::".join([krate, *segs]))
    return sorted(set(out))


def fn_name(rng: random.Random, krate: str, module: str, serial: int) -> str:
    r = rng.random()
    if r < 0.07:
        return f"<{module}::{rng.choice(TYPES)} as {rng.choice(TRAITS)}"
    if r < 0.10:
        return f"<alloc::vec::Vec<{module}::{rng.choice(TYPES)}> as core::ops::drop::Drop>::drop"
    if r < 0.14:
        return f"core::ptr::drop_in_place::<{module}::{rng.choice(TYPES)}{serial}>"
    if r < 0.20:
        return f"{module}::{rng.choice(VERBS)}_{rng.choice(WORDS)}::{{closure#{rng.randrange(3)}}}"
    if r < 0.30:
        return f"<{module}::{rng.choice(TYPES)}>::{rng.choice(VERBS)}_{serial}"
    return f"{module}::{rng.choice(VERBS)}_{rng.choice(WORDS)}_{serial}"


@dataclasses.dataclass
class Fn:
    key: int
    krate: int
    module: int
    how: int
    ns: int
    build: int
    name: str


def generate(inp: Input) -> tuple[list[str], list[Fn], list[tuple[int, int]], list[list[int]]]:
    rng = random.Random(inp.seed)
    crates = [c for c, _ in CRATES]
    total_share = sum(s for _, s in CRATES)
    sizes = [max(5, int(inp.nodes * s / total_share)) for _, s in CRATES]
    sizes[0] += inp.nodes - sum(sizes)
    fns: list[Fn] = []
    by_module: list[list[int]] = []
    module_crate: list[int] = []
    keys: set[int] = set()
    for ci, (krate, size) in enumerate(zip(crates, sizes)):
        mods = modules_of(rng, krate, size)
        weights = [rng.paretovariate(1.2) for _ in mods]
        wsum = sum(weights)
        counts = [max(1, int(size * w / wsum)) for w in weights]
        counts[0] += size - sum(counts)
        if counts[0] < 1:
            raise SystemExit("fixture.py: internal: module sizes do not add up")
        for module, count in zip(mods, counts):
            mi = len(by_module)
            by_module.append([])
            module_crate.append(ci)
            for _ in range(count):
                key = rng.getrandbits(64)
                while key in keys:
                    key = rng.getrandbits(64)
                keys.add(key)
                if krate in SYSTEM:
                    how = rng.choices((HOW_STORED, HOW_COMPILED, HOW_UNIT), (0.8, 0.15, 0.05))[0]
                else:
                    how = rng.choices((HOW_COMPILED, HOW_UNIT, HOW_UNCHANGED, HOW_STORED), (0.72, 0.06, 0.12, 0.10))[0]
                ns = int(rng.lognormvariate(math.log(180_000), 1.3)) if how == HOW_COMPILED else 0
                build = inp.build if how in (HOW_COMPILED, HOW_UNIT) else rng.randrange(1, inp.build)
                by_module[mi].append(len(fns))
                fns.append(Fn(key, ci, mi, how, ns, build, fn_name(rng, krate, module, len(fns))))

    # Power-law out- and in-degree: callers drawn by a Pareto weight, callees Zipf-like within
    # their module; most calls stay in the module or crate, the rest go to std/core/alloc.
    n = len(fns)
    out_w = [rng.paretovariate(1.5) for _ in range(n)]
    cum = []
    acc = 0.0
    for w in out_w:
        acc += w
        cum.append(acc)
    crate_nodes: list[list[int]] = [[] for _ in crates]
    for i, f in enumerate(fns):
        crate_nodes[f.krate].append(i)
    rank = [RANK[c] for c in crates]
    deeper_system = [[i for s in SYSTEM if rank[crates.index(s)] > rank[c] for i in crate_nodes[crates.index(s)]] for c in range(len(crates))]
    deeper = [[i for d in range(len(crates)) if rank[d] > rank[c] and crates[d] not in SYSTEM for i in crate_nodes[d]] for c in range(len(crates))]

    def zipf_pick(pool: list[int]) -> int:
        return pool[min(len(pool) - 1, int(len(pool) * rng.random() ** 2.5))]

    edges: set[tuple[int, int]] = set()
    attempts = 0
    while len(edges) < inp.edges and attempts < inp.edges * 4:
        attempts += 1
        src = bisect_cum(cum, rng.random() * acc)
        f = fns[src]
        r = rng.random()
        if r < 0.5:
            pool = by_module[f.module]
        elif r < 0.75:
            pool = crate_nodes[f.krate]
        elif r < 0.93:
            pool = deeper_system[f.krate] or crate_nodes[f.krate]
        elif r < 0.99 or crates[f.krate] not in SYSTEM:
            pool = deeper[f.krate] or deeper_system[f.krate] or crate_nodes[f.krate]
        else:
            # std's generic code calling back into the impls it was instantiated with.
            pool = crate_nodes[rng.randrange(len(crates))]
        dst = zipf_pick(pool)
        if dst != src:
            edges.add((src, dst))
    return crates, fns, sorted(edges), by_module


def bisect_cum(cum: list[float], x: float) -> int:
    lo, hi = 0, len(cum) - 1
    while lo < hi:
        mid = (lo + hi) // 2
        if cum[mid] < x:
            lo = mid + 1
        else:
            hi = mid
    return lo


def put_str(buf: bytearray, s: str) -> None:
    b = s.encode()
    buf += struct.pack("<I", len(b))
    buf += b


def snapshot(inp: Input, crates: list[str], fns: list[Fn], edges: list[tuple[int, int]]) -> bytes:
    buf = bytearray(MAGIC)
    buf += struct.pack("<I", VERSION)
    put_str(buf, inp.root)
    buf += struct.pack("<Q", inp.build)
    buf += struct.pack("<I", len(crates))
    for c in crates:
        put_str(buf, c)
    buf += struct.pack("<I", len(fns))
    for f in fns:
        buf += struct.pack("<QIBQQ", f.key, f.krate, f.how, f.ns, f.build)
        put_str(buf, f.name)
    buf += struct.pack("<I", len(edges))
    for a, b in edges:
        buf += struct.pack("<II", a, b)
    return bytes(buf)


def edit_records(inp: Input, crates: list[str], fns: list[Fn], edges: list[tuple[int, int]]) -> list[dict[str, Any]]:
    """A body edit in the app crate: `edited` functions of one file, and the callers that
    inline them compiled again, reached by walking callers out from the edited ones."""
    rng = random.Random(inp.seed + 1)
    callers: dict[int, list[int]] = {}
    for a, b in edges:
        callers.setdefault(b, []).append(a)
    app = [i for i, f in enumerate(fns) if crates[f.krate] == "anim" and f.how == HOW_COMPILED and callers.get(i)]
    edited = rng.sample(app, inp.edited)
    order = list(edited)
    via: dict[int, int] = {}
    seen = set(edited)
    frontier = list(edited)
    while frontier and len(order) < inp.recompiled:
        nxt = []
        for x in frontier:
            for c in callers.get(x, []):
                if c not in seen and fns[c].krate == fns[x].krate and len(order) < inp.recompiled:
                    seen.add(c)
                    via[c] = x
                    order.append(c)
                    nxt.append(c)
        frontier = nxt
    build = inp.build + 1
    file = "src/" + fns[edited[0]].name.split("::")[1].strip("<>{}") + ".rs"
    t = 0

    def rec(ev: Any) -> dict[str, Any]:
        nonlocal t
        t += rng.randrange(200_000, 3_000_000)
        return {"at_ns": t, "event": ev}

    ids = {node: i for i, node in enumerate(order)}
    in_set = set(order)
    out = [
        rec({"Began": {"build": build, "command": "build", "root": inp.root}}),
        rec({"Started": {"step": "LoadWorkspace", "of": None}}),
        rec({"Ended": {"step": "LoadWorkspace", "of": None}}),
        rec({"Started": {"step": "FrontEnd", "of": "anim"}}),
        rec({"Ended": {"step": "FrontEnd", "of": "anim"}}),
        rec({"Started": {"step": "Codegen", "of": "anim"}}),
    ]
    calls: dict[int, list[int]] = {}
    for a, b in edges:
        if a in in_set and b in in_set:
            calls.setdefault(a, []).append(ids[b])
    fn_list = []
    for node in order:
        f = fns[node]
        fn_list.append({
            "id": ids[node], "key": f.key, "krate": crates[f.krate], "name": f.name,
            "calls": calls.get(node, []),
            "how": {"Compiled": {"ns": int(rng.lognormvariate(math.log(400_000), 0.8))}},
        })
    new_name = fns[edited[0]].name.rsplit("::", 1)[0] + "::helper_added_by_edit"
    new_id = len(order)
    fn_list.append({"id": new_id, "key": rng.getrandbits(64), "krate": "anim", "name": new_name,
                    "calls": [], "how": {"Compiled": {"ns": 250_000}}})
    fn_list[0]["calls"].append(new_id)
    out.append(rec({"Functions": {"object": 3, "whole": False, "functions": fn_list}}))
    for node in order:
        f = fns[node]
        if node in via:
            cause: Any = {"Callee": {"name": fns[via[node]].name}}
        else:
            cause = {"Edited": {"file": file}}
        out.append(rec({"Rebuilt": {"name": f.name, "krate": crates[f.krate], "cause": cause}}))
    out.append(rec({"Rebuilt": {"name": new_name, "krate": "anim", "cause": {"Edited": {"file": file}}}}))
    out += [
        rec({"Ended": {"step": "Codegen", "of": "anim"}}),
        rec({"Started": {"step": "Link", "of": "anim"}}),
        rec({"Ended": {"step": "Link", "of": "anim"}}),
        rec({"Finished": {"ok": True, "summary": {
            "took_ns": t, "crates": 1,
            "fns": {"total": len(fns) + 1, "done": len(fn_list), "kept": 0},
            "rebuilt": [], "more": 0, "error": None,
            "cache": {"found": 0, "fetched": 0, "compiled": len(fn_list), "cold": None},
            "slowest": [{"step": "Codegen", "of": "anim", "took_ns": t}]}}}),
    ]
    return out


def cbor_head(major: int, n: int) -> bytes:
    if n < 24:
        return bytes([major << 5 | n])
    for info, fmt in ((24, ">B"), (25, ">H"), (26, ">I"), (27, ">Q")):
        if n < 1 << (8 * struct.calcsize(fmt)):
            return bytes([major << 5 | info]) + struct.pack(fmt, n)
    raise ValueError(f"{n} does not fit a CBOR head")


def cbor(v: Any) -> bytes:
    """The deterministic encoding (RFC 8949 4.2.1): shortest heads, map keys sorted by bytes."""
    if v is None:
        return b"\xf6"
    if v is True:
        return b"\xf5"
    if v is False:
        return b"\xf4"
    if isinstance(v, int):
        return cbor_head(0, v) if v >= 0 else cbor_head(1, -1 - v)
    if isinstance(v, str):
        b = v.encode()
        return cbor_head(3, len(b)) + b
    if isinstance(v, list):
        return cbor_head(4, len(v)) + b"".join(cbor(x) for x in v)
    if isinstance(v, dict):
        items = sorted((cbor(k), cbor(x)) for k, x in v.items())
        return cbor_head(5, len(items)) + b"".join(k + x for k, x in items)
    raise TypeError(f"no CBOR for {type(v).__name__}")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inp = parse_input(sys.argv[1])
    crates, fns, edges, _ = generate(inp)
    out = Path(inp.out)
    out.write_bytes(snapshot(inp, crates, fns, edges))
    edit = out.with_name(out.stem + ".edit.cbor")
    edit.write_bytes(b"".join(cbor(r) for r in edit_records(inp, crates, fns, edges)))
    print(f"{out}: {len(crates)} crates, {len(fns)} functions, {len(edges)} calls; {edit}")


if __name__ == "__main__":
    main()
