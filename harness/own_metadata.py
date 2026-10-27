#!/usr/bin/env python3
"""own_metadata.py: the `cargo metadata` blaze-metadata makes of a workspace, with no cargo,
against cargo's, over a corpus. cargo is the oracle; blaze never runs it outside this harness.

usage: own_metadata.py CORPUS [PART...] [--work DIR] [--jobs N] [--out FILE] [--limit N] [--keep]
                       [--only NAME,...]

CORPUS and its parts are those of harness/resolve_graph.py. Each case with a Cargo.lock is
fetched under `--work` (default $TMPDIR/blaze-own-metadata) and checked twice, both for the
host and with the lock file it ships:

- `metadata`: the `metadata` example of blaze-metadata, run first with a CARGO_HOME of its own
  so that it fetches every package itself, against `cargo metadata --format-version 1
  --filter-platform HOST` run with another; the whole JSON is compared, key order included.
- `build`: the example with `--build`, the graph of a plain `cargo build` in the workspace's
  root, against `cargo build --unit-graph`: the packages it builds, each one's features (over
  all its units) and the packages each one's units depend on; and with `--programs`, the
  binaries it links (the unit graph's roots that are executables), as blaze's whole build
  takes them.

A case is equal, different, blaze-failed or cargo-failed; the first difference or error of
each that is not equal is printed, and `--out` writes them all as JSON.
"""
import concurrent.futures, json, os, shutil, subprocess, sys, tempfile, tomllib

import resolve_graph as rg

CLASSES = rg.CLASSES
RUSTC = os.environ.get("RUSTC_ORACLE", f"rustc +{rg.PIN}").split()


def build_blaze():
    target = os.environ.get("CARGO_TARGET_DIR", os.path.join(rg.ROOT, "target"))
    subprocess.run([*rg.CARGO, "build", "--release", "-q", "-p", "blaze-metadata", "--example",
                    "metadata"], cwd=rg.ROOT, check=True)
    return os.path.join(target, "release", "examples", "metadata")


def host(work):
    triple = next(l.split()[1] for l in subprocess.run([*RUSTC, "-vV"], capture_output=True,
                  text=True, check=True).stdout.splitlines() if l.startswith("host:"))
    cfg = os.path.join(work, "host.cfg")
    with open(cfg, "w") as f:
        f.write(subprocess.run([*RUSTC, "--print", "cfg"], capture_output=True, text=True,
                               check=True).stdout)
    return triple, cfg


def unit_graph(graph):
    """What a unit graph builds: each package's features, over all its units, and the packages
    its units depend on, the running of build scripts left out."""
    units = graph["units"]
    out = {}
    for u in units:
        node = out.setdefault(u["pkg_id"], {"features": set(), "deps": set()})
        node["features"] |= set(u["features"])
        if u["mode"] == "run-custom-build":
            continue
        for d in u["dependencies"]:
            dep = units[d["index"]]
            if dep["pkg_id"] != u["pkg_id"] and dep["mode"] != "run-custom-build":
                node["deps"].add(dep["pkg_id"])
    return out


def unit_programs(graph):
    """What a unit graph links: its roots that build a binary or an example, or a library's
    `cdylib` or `staticlib`."""
    units = graph["units"]
    roots = (units[at] for at in graph["roots"])
    linked = {"cdylib", "staticlib"}
    return {(u["pkg_id"], u["target"]["name"]) for u in roots
            if u["mode"] == "build"
            and ((u["target"]["crate_types"] == ["bin"] and set(u["target"]["kind"]) & {"bin", "example"})
                 or (set(u["target"]["crate_types"]) & linked and not set(u["target"]["kind"]) & {"example", "bench", "test"}))}


def of_resolve(metadata):
    return {n["id"]: {"features": set(n["features"]), "deps": {d["pkg"] for d in n["deps"]}}
            for n in metadata["resolve"]["nodes"]}


def graph_difference(ours, theirs):
    if set(ours) != set(theirs):
        only_b = sorted(set(theirs) - set(ours))[:3]
        only_a = sorted(set(ours) - set(theirs))[:3]
        return f"packages only cargo's {only_b}, only blaze's {only_a}"
    for pkg in sorted(theirs):
        for key in ["features", "deps"]:
            a, b = ours[pkg][key], theirs[pkg][key]
            if a != b:
                return f"{pkg} {key}: only cargo's {sorted(b - a)[:5]}, only blaze's {sorted(a - b)[:5]}"
    return None


def rustflags(case_dir, triple):
    """The `--cfg` flags of the build's rustflags, as cargo passes them to `rustc --print cfg`:
    `[target.<triple>]`'s, else `[build]`'s, of the case's `.cargo/config.toml`."""
    config = os.path.join(case_dir, ".cargo", "config.toml")
    if not os.path.exists(config):
        config = os.path.join(case_dir, ".cargo", "config")
    if not os.path.exists(config):
        return []
    try:
        table = tomllib.load(open(config, "rb"))
    except tomllib.TOMLDecodeError:
        return []
    flags = table.get("target", {}).get(triple, {}).get("rustflags")
    if flags is None:
        flags = table.get("build", {}).get("rustflags", [])
    if isinstance(flags, str):
        flags = flags.split()
    return flags


def case_cfg(case, platform):
    """The host's `cfg` for the case: rustc's with the case's rustflags."""
    triple, cfg = platform
    flags = rustflags(case["dir"], triple)
    if not flags:
        return cfg
    printed = subprocess.run([*RUSTC, "--print", "cfg", *flags], capture_output=True, text=True)
    if printed.returncode:
        return cfg
    path = case["dir"] + ".cfg"
    with open(path, "w") as f:
        f.write(printed.stdout)
    return path


def one(case, blaze, homes, platform):
    lock = os.path.join(case["dir"], "Cargo.lock")
    shipped = lock + ".shipped"
    shutil.copyfile(lock, shipped)
    env = {k: v for k, v in os.environ.items() if not k.startswith("CARGO_") or k == "CARGO_ORACLE"}
    triple, cfg = platform[0], case_cfg(case, platform)
    run = lambda cmd, home, **kw: subprocess.run(cmd, cwd=case["dir"],
                                                 env={**env, "CARGO_HOME": home, **kw},
                                                 capture_output=True, text=True, errors="replace")
    put_back = lambda: shutil.copyfile(shipped, lock)
    results = []
    try:
        for mode in ["metadata", "build"]:
            out = {"part": case["part"], "name": case["name"], "mode": mode}
            flags = ["--filter-platform", triple, "--cfg-file", cfg]
            ours = run([blaze, *flags, *(["--build"] if mode == "build" else [])], homes["blaze"])
            put_back()
            if mode == "metadata":
                cargo = run([*rg.CARGO, "metadata", "--format-version", "1", "--filter-platform",
                             triple], homes["cargo"])
            else:
                cargo = run([*rg.CARGO, "build", "--unit-graph", "-Z", "unstable-options"],
                            homes["cargo"], RUSTC_BOOTSTRAP="1")
            put_back()
            if cargo.returncode:
                lines = [l for l in cargo.stderr.splitlines() if l.startswith("error")]
                why = (lines or [cargo.stderr.strip()])[0][:300]
                results.append({**out, "class": "cargo-failed", "why": why})
                continue
            if ours.returncode:
                why = (ours.stderr.strip().splitlines() or ["no output"])[-1][:300]
                results.append({**out, "class": "blaze-failed", "why": why})
                continue
            ours_text = ours.stdout.replace(homes["blaze"], homes["cargo"])
            if mode == "metadata":
                diff = rg.first_difference(json.loads(ours_text), json.loads(cargo.stdout))
            else:
                theirs = json.loads(cargo.stdout)
                diff = graph_difference(of_resolve(json.loads(ours_text)), unit_graph(theirs))
                listed = run([blaze, *flags, "--build", "--programs"], homes["blaze"])
                put_back()
                if not diff and listed.returncode:
                    diff = "programs: " + (listed.stderr.strip().splitlines() or ["no output"])[-1][:300]
                elif not diff:
                    ours_programs = {(it["package"], it["name"]) for it in json.loads(listed.stdout)}
                    programs = unit_programs(theirs)
                    if ours_programs != programs:
                        diff = (f"programs only cargo's {sorted(programs - ours_programs)[:3]}, "
                                f"only blaze's {sorted(ours_programs - programs)[:3]}")
            results.append({**out, "class": "different" if diff else "equal", "why": diff})
    finally:
        os.replace(shipped, lock)
        if cfg != platform[1]:
            os.remove(cfg)
    return results


def main():
    argv = sys.argv[1:]
    work = rg.option(argv, "--work", os.path.join(tempfile.gettempdir(), "blaze-own-metadata"))
    jobs = int(rg.option(argv, "--jobs", "8"))
    out_file = rg.option(argv, "--out")
    limit = rg.option(argv, "--limit")
    limit = int(limit) if limit else None
    keep = "--keep" in argv and not argv.remove("--keep")
    only = rg.option(argv, "--only")
    corpus = json.load(open(argv.pop(0)))
    parts = argv or rg.PARTS
    blaze = build_blaze()
    homes = {who: os.path.join(work, f"{who}-home") for who in ["blaze", "cargo"]}
    for home in homes.values():
        os.makedirs(home, exist_ok=True)
    platform = host(work)
    cases = [c for part in parts for c in rg.cases_of(part, corpus, work, limit)]
    if only:
        cases = [c for c in cases if c["name"] in only.split(",")]

    def run(case):
        try:
            if not os.path.exists(case["dir"]):
                case["fetch"]()
        except Exception as e:
            return [{"part": case["part"], "name": case["name"], "mode": "fetch",
                     "class": "cargo-failed", "why": f"fetch: {e}"[:300]}]
        if not os.path.exists(os.path.join(case["dir"], "Cargo.lock")):
            results = []
        else:
            results = one(case, blaze, homes, platform)
        if not keep:
            shutil.rmtree(case["dir"], ignore_errors=True)
        return results

    results = []
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for rs in pool.map(run, cases):
            for r in rs:
                results.append(r)
                if r["class"] != "equal":
                    print(f"{r['class']:13} {r['mode']:8} {r['part']}/{r['name']}: {r['why']}", flush=True)
    for mode in ["metadata", "build"]:
        counts = {c: sum(1 for r in results if r["mode"] == mode and r["class"] == c) for c in CLASSES}
        print(f"{mode}: " + ", ".join(f"{n} {c}" for c, n in counts.items()))
    if out_file:
        json.dump(results, open(out_file, "w"), indent=1)


if __name__ == "__main__":
    main()
