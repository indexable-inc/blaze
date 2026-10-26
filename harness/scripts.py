#!/usr/bin/env python3
"""scripts.py: build scripts run by blaze (crates/blaze-build-script) against cargo's runs of them.

usage: scripts.py [--work DIR] [--json OUT] [--jobs N] [--clean] [name ...]

The corpus is the applications of apps/apps.json (bun aside, whose build needs its own C++
first) and scripts/, a package that depends on crates whose scripts those do not run
(openssl-sys, zstd-sys, libz-sys, ring, rustix, num-traits through autocfg). With no name, all.

The pinned nightly's cargo checks each with this file as its RUSTC_WRAPPER, which puts blaze-build-script
where rustc wrote each build script: run by cargo, it writes what cargo gave it beside
`OUT_DIR` and becomes the script. Then each run is described from `cargo metadata`, `cargo
check --unit-graph` and rustc, as blaze's resolver will describe it, and blaze-build-script

  env    computes the script's environment, compared with what cargo gave it;
  parse  reads cargo's `output` of the run, compared with what cargo said it read
         (its `build-script-executed` message: libraries, search paths, cfgs, variables);
  run    runs the script again into an `OUT_DIR` of its own and judges the run, compared with
         cargo's: both fail, or both succeed and come to the same (blaze's reading of each
         output); `stdout` counts those whose bytes are the same too.

blaze's `OUT_DIR` is named as cargo's in what is compared. The `DEP_<links>_<key>` variables
of each environment come from blaze's reading of the dependency's output, so `env` checks
metadata too. A line per project and a total; --json writes every difference.
"""
import argparse, concurrent.futures, json, os, re, shutil, subprocess, sys, tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SPECS = os.path.join(HERE, "apps", "apps.json")
PIN = open(os.path.join(HERE, "toolchain")).read().strip()
TOOLCHAIN = PIN
# Set in cargo's environment: the wrapper's cue, and the program it puts in place of scripts.
WRAP = "BLAZE_SCRIPTS_WRAP"
LEFT_OUT = {"bun"}
UNIT_HASH = re.compile(r"[0-9a-f]{16}")


def wrap(argv):
    """As cargo's RUSTC_WRAPPER: rustc compiles; a build script it wrote is moved aside and
    blaze-build-script put in its place, told where the script went (`harness-real`)."""
    args = argv[1:]

    def value(flag):
        for i, arg in enumerate(args):
            if arg == flag and i + 1 < len(args):
                return args[i + 1]
            if arg.startswith(flag + "="):
                return arg[len(flag) + 1:]
        return None

    name = value("--crate-name") or ""
    if not name.startswith("build_script_"):
        os.execv(argv[0], argv)
    status = subprocess.call(argv)
    extra = next((arg.split("=", 1)[1] for i, arg in enumerate(args)
                  if arg.startswith("extra-filename=") and args[i - 1] == "-C"), "")
    extra = next((arg.split("=", 1)[1] for arg in args if arg.startswith("-Cextra-filename=")), extra)
    out_dir = value("--out-dir")
    if status == 0 and out_dir:
        script = os.path.join(out_dir, name + extra)
        if os.path.isfile(script):
            os.replace(script, script + ".real")
            shutil.copy2(os.environ[WRAP], script)
            with open(os.path.join(out_dir, "harness-real"), "w") as f:
                f.write(script + ".real")
    sys.exit(status)


def unit_dirs(profile):
    """The directory of each unit cargo built for `profile`, which holds the files of a build
    script's run around its `OUT_DIR`: `build/<package>-<hash>` in cargo's first layout of the
    build directory, `build/<package>/<hash>` in its second (nightly cargo's default)."""
    build = os.path.join(profile, "build")
    found = []
    for name in sorted(os.listdir(build)) if os.path.isdir(build) else []:
        first = os.path.join(build, name)
        found.append(first)
        found += [os.path.join(first, h) for h in sorted(os.listdir(first))
                  if UNIT_HASH.fullmatch(h) and os.path.isdir(os.path.join(first, h, "out"))]
    return found


def printed(run_dir):
    """The file cargo kept what a build script printed in: `output` in the first layout,
    `run/stdout` in the second."""
    second = os.path.join(run_dir, "run", "stdout")
    return second if os.path.isfile(second) else os.path.join(run_dir, "output")


def host_deps(profile):
    """Where cargo has a build script load the host's dynamic libraries from: `deps` in the
    first layout, the `out` of each unit that built a `dylib` in the second."""
    deps = os.path.join(profile, "deps")
    if os.path.isdir(deps) or not os.path.isdir(os.path.join(profile, "build")):
        return [deps]
    suffix = {"darwin": ".dylib", "win32": ".dll"}.get(sys.platform, ".so")
    return [out for d in unit_dirs(profile) if os.path.isdir(out := os.path.join(d, "out"))
            and any(f.endswith(suffix) and not f.endswith(f"-{os.path.basename(d)}{suffix}")
                    for f in os.listdir(out))]


def sh(argv, cwd, env, check=True):
    ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, errors="replace")
    if check and ran.returncode != 0:
        raise SystemExit(f"`{' '.join(argv)}` failed in {cwd}:\n{(ran.stdout + ran.stderr)[-3000:]}")
    return ran.stdout


def tools():
    """cargo, rustc and rustdoc of the toolchain, run directly: rustup's proxies would add to
    the environment."""
    cargo = os.path.realpath(sh(["rustup", "which", "--toolchain", TOOLCHAIN, "cargo"], ROOT, None).strip())
    bin_dir = os.path.dirname(cargo)
    rustc = os.path.join(bin_dir, "rustc")
    verbose = sh([rustc, "-vV"], ROOT, None)
    return {
        "cargo": cargo,
        "rustc": rustc,
        "rustdoc": os.path.join(bin_dir, "rustdoc"),
        "host": re.search(r"^host: (\S+)", verbose, re.M).group(1),
        "libdir": sh([rustc, "--print", "target-libdir"], ROOT, None).strip(),
    }


def blaze_build_script():
    """blaze-build-script, built from this checkout."""
    out = sh(["cargo", f"+{TOOLCHAIN}", "build", "--release", "-p", "blaze-build-script",
              "--message-format=json"], ROOT, None)
    for line in out.splitlines():
        message = json.loads(line) if line.startswith("{") else {}
        if message.get("executable") and message["target"]["name"] == "blaze-build-script":
            return message["executable"]
    raise SystemExit("cargo built no blaze-build-script")


def checkout(name, spec, work):
    ws = os.path.join(work, "src", name)
    if "source" in spec:
        shutil.rmtree(ws, ignore_errors=True)
    if not os.path.isdir(ws):
        os.makedirs(ws)
        if "repo" in spec:
            for argv in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", spec["repo"], spec["rev"]],
                         ["git", "checkout", "-q", "FETCH_HEAD"],
                         ["git", "submodule", "update", "-q", "--init", "--recursive", "--depth", "1"]):
                sh(argv, ws, None)
        else:
            shutil.copytree(spec["source"], ws, dirs_exist_ok=True)
    return os.path.join(ws, spec.get("dir", ""))


def corpus():
    with open(SPECS) as f:
        specs = {name: spec for name, spec in json.load(f).items() if name not in LEFT_OUT}
    for spec in specs.values():
        if "source" in spec:
            spec["source"] = os.path.join(HERE, "apps", spec["source"])
    specs["scripts"] = {"source": os.path.join(HERE, "scripts")}
    return specs


def cargo_env(t, target_dir, blaze, spec):
    """What cargo runs with: this environment without what names a toolchain or a build."""
    dropped = ("CARGO_", "RUSTUP_", "RUSTC", "RUSTDOC", "RUSTFLAGS", "RUST_")
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(dropped) or k == "CARGO_HOME"}
    for name in ("MAKEFLAGS", "MFLAGS", "OUT_DIR", "TARGET", "HOST", "NUM_JOBS", "OPT_LEVEL", "DEBUG", "PROFILE"):
        env.pop(name, None)
    env.update(spec.get("build_env", {}))
    env.update({
        "RUSTC": t["rustc"],
        "RUSTDOC": t["rustdoc"],
        "RUSTC_WRAPPER": os.path.abspath(__file__),
        WRAP: blaze,
        "CARGO_TARGET_DIR": target_dir,
    })
    return env


def configs(ws):
    """cargo's configuration files that apply in `ws`, lowest precedence first (CARGO_HOME's,
    then those of the directories down to the workspace's): (the directory a relative path is
    of, the file's table)."""
    dirs = []
    d = os.path.abspath(ws)
    while True:
        dirs.append(d)
        parent = os.path.dirname(d)
        if parent == d:
            break
        d = parent
    cargo_home = os.environ.get("CARGO_HOME") or os.path.join(os.path.expanduser("~"), ".cargo")
    places = [(os.path.dirname(cargo_home), cargo_home)] + [(d, os.path.join(d, ".cargo")) for d in reversed(dirs)]
    found = []
    for d, config_dir in places:
        for name in ("config", "config.toml"):
            path = os.path.join(config_dir, name)
            if os.path.isfile(path):
                with open(path, "rb") as f:
                    found.append((d, tomllib.load(f)))
    return found


def config_env(ws):
    """`[env]`, the nearer file winning: a list of what blaze-build-script takes."""
    found = {}
    for d, table in configs(ws):
        for key, value in table.get("env", {}).items():
            if isinstance(value, dict):
                text = value.get("value", "")
                if value.get("relative"):
                    text = os.path.join(d, text)
                found[key] = {"name": key, "value": text, "force": bool(value.get("force"))}
            else:
                found[key] = {"name": key, "value": str(value), "force": False}
    return list(found.values())


def cfg_matches(expr, cfgs):
    """Whether a `cfg(..)` expression holds of rustc's `--print cfg` lines."""
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', expr)
    pos = 0

    def take():
        nonlocal pos
        pos += 1
        return tokens[pos - 1]

    def predicate():
        name = take()
        if name in ("all", "any", "not") and pos < len(tokens) and tokens[pos] == "(":
            take()
            args = []
            while tokens[pos] != ")":
                args.append(predicate())
                if tokens[pos] == ",":
                    take()
            take()
            return all(args) if name == "all" else any(args) if name == "any" else not args[0]
        if pos < len(tokens) and tokens[pos] == "=":
            take()
            return f"{name}={take()}" in cfgs
        return name in cfgs

    return predicate()


def config_rustflags(ws, host, cfgs):
    """The flags cargo gives rustc for the host when RUSTFLAGS is not set: those of
    `target.<host>` and of each `target.'cfg(..)'` that holds, else `build.rustflags`, each
    list joined across the files, the nearer later."""
    def flags(value):
        return value.split() if isinstance(value, str) else list(value)

    tables = [table for _, table in configs(ws)]
    target = []
    for table in tables:
        targets = table.get("target", {})
        target += flags(targets.get(host, {}).get("rustflags", []))
        for key in sorted(targets):
            if key.startswith("cfg(") and key.endswith(")") and cfg_matches(key[4:-1], cfgs):
                target += flags(targets[key].get("rustflags", []))
    if target:
        return target
    return [f for table in tables for f in flags(table.get("build", {}).get("rustflags", []))]


def envify(name):
    return name.upper().replace("-", "_")


def debuginfo_on(value):
    return value not in (None, 0, False, "0", "none", "false")


def package_of(meta_package, descr=None):
    targets = [{"name": t["name"], "kind": t["kind"], "crate_types": t["crate_types"]}
               for t in meta_package["targets"]]
    return {
        "name": meta_package["name"],
        "version": meta_package["version"],
        "descr": descr,
        "manifest_dir": os.path.dirname(meta_package["manifest_path"]),
        "manifest_path": meta_package["manifest_path"],
        "authors": meta_package.get("authors") or [],
        "description": meta_package.get("description"),
        "homepage": meta_package.get("homepage"),
        "repository": meta_package.get("repository"),
        "license": meta_package.get("license"),
        "license_file": meta_package.get("license_file"),
        "readme": meta_package.get("readme"),
        "rust_version": meta_package.get("rust_version"),
        "links": meta_package.get("links"),
        "targets": targets,
    }


def for_host(target):
    return "custom-build" in target["kind"] or "proc-macro" in target["kind"]


def linkable(target):
    return any(k in ("lib", "rlib", "dylib", "proc-macro") for k in target["kind"])


def project(name, spec, work, t, blaze, jobs, clean):
    ws = checkout(name, spec, work)
    target_dir = os.path.join(work, "target", name)
    # Every script runs, under the environment of this run of the harness.
    shutil.rmtree(target_dir, ignore_errors=True)
    env = cargo_env(t, target_dir, blaze, spec)
    args = spec.get("cargo_args", [])
    checked = subprocess.run([t["cargo"], "check", "--keep-going", "--message-format=json", "-j", str(jobs), *args],
                             cwd=ws, env=env, capture_output=True, text=True, errors="replace")
    executed = {}
    for line in checked.stdout.splitlines():
        message = json.loads(line) if line.startswith("{") else {}
        if message.get("reason") == "build-script-executed":
            executed[os.path.realpath(message["out_dir"])] = message
    graph = json.loads(sh([t["cargo"], "check", "--unit-graph", "-Z", "unstable-options", *args], ws,
                          {**env, "RUSTC_BOOTSTRAP": "1"}))
    meta = json.loads(sh([t["cargo"], "metadata", "--format-version", "1"], ws, env))
    packages = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    units = graph["units"]
    host_root = os.path.join(target_dir, "debug")
    configured = config_env(ws)
    rustflags = config_rustflags(ws, t["host"], set(sh([t["rustc"], "--print", "cfg"], ws, None).splitlines()))
    target_cfgs = sh([t["rustc"], "--print", "cfg", *rustflags], ws, None).splitlines()

    recordings = []
    for run_dir in unit_dirs(host_root):
        path = os.path.join(run_dir, "harness-ran.json")
        if os.path.isfile(path):
            with open(path) as f:
                recordings.append((run_dir, json.load(f)))

    # Which run of the unit graph each recording is.
    run_of = {}
    used = set()
    for i, unit in enumerate(units):
        if unit["mode"] != "run-custom-build":
            continue
        package = packages[unit["pkg_id"]]
        manifest_dir = os.path.dirname(package["manifest_path"])
        features = {"CARGO_FEATURE_" + envify(f) for f in unit["features"]}
        for k, (run_dir, rec) in enumerate(recordings):
            renv = rec["env"]
            if (k not in used and renv.get("CARGO_MANIFEST_DIR") == manifest_dir
                    and {n for n in renv if n.startswith("CARGO_FEATURE_")} == features
                    and renv.get("OPT_LEVEL") == unit["profile"]["opt_level"]
                    and renv.get("DEBUG") == ("true" if debuginfo_on(unit["profile"]["debuginfo"]) else "false")):
                used.add(k)
                run_of[i] = k
                break

    def version_key(v):
        core = re.split(r"[-+]", v)[0]
        return tuple(int(x) if x.isdigit() else 0 for x in core.split("."))

    def package_key(i):
        p = packages[units[i]["pkg_id"]]
        return (p["name"], version_key(p["version"]), p["version"], p.get("source") or "")

    memo = {}

    def to_link(i):
        """The script runs unit `i` is linked with (cargo's `build_map`): a list of (unit of the
        run, its run directory), in cargo's order."""
        if i in memo:
            return memo[i]
        unit = units[i]
        ret = []

        def add(entry):
            if entry not in ret:
                ret.append(entry)

        deps = [d["index"] for d in unit["dependencies"]]
        if "custom-build" not in unit["target"]["kind"]:
            for d in deps:
                if units[d]["mode"] == "run-custom-build" and units[d]["pkg_id"] == unit["pkg_id"]:
                    add(d)
        for d in sorted(deps, key=package_key):
            if for_host(units[d]["target"]):
                continue
            if linkable(units[d]["target"]):
                for entry in to_link(d):
                    add(entry)
        memo[i] = ret
        return ret

    def run_hash(i):
        k = run_of.get(i)
        return os.path.basename(recordings[k][0]) if k is not None else ""

    blaze_dir = os.path.join(work, "blaze", name)
    shutil.rmtree(blaze_dir, ignore_errors=True)
    os.makedirs(blaze_dir)
    with open(os.path.join(blaze_dir, "cargo-env.json"), "w") as f:
        json.dump(env, f)

    def unit_json(i, out_dir, dependencies, plugin_search):
        unit = units[i]
        run_dir, rec = recordings[run_of[i]]
        profile = unit["profile"]
        makeflags = rec["env"].get("CARGO_MAKEFLAGS", "")
        auth = re.search(r"--jobserver-auth=(\S+)", makeflags)
        with open(os.path.join(os.path.dirname(rec["args"][0]), "harness-real")) as f:
            real = f.read().strip()
        return {
            "package": package_of(packages[unit["pkg_id"]]),
            "features": unit["features"],
            "profile": {
                "name": profile["name"],
                "release": profile["name"] in ("release", "bench"),
                "opt_level": str(profile["opt_level"]),
                "debuginfo": debuginfo_on(profile["debuginfo"]),
                "debug_assertions": bool(profile["debug_assertions"]),
            },
            "target": t["host"],
            "host": t["host"],
            "target_cfgs": target_cfgs,
            "rustflags": rustflags,
            "jobs": jobs,
            "script": real,
            "out_dir": out_dir,
            "tools": {
                "cargo": t["cargo"],
                "rustc": t["rustc"],
                "rustdoc": t["rustdoc"],
                "rustc_wrapper": os.path.abspath(__file__),
            },
            "workspace_member": unit["pkg_id"] in members,
            "dependencies": dependencies,
            "config_env": configured,
            "dylib_search": {
                # `cargo check` writes nothing to the artifact directory, and has none.
                "host_root": None,
                "host_deps": host_deps(host_root),
                "sysroot_libdir": t["libdir"],
                "plugin_search": plugin_search,
            },
            "jobserver": auth.group(1) if auth else None,
        }

    def call(*argv):
        ran = subprocess.run([blaze, *argv], capture_output=True, text=True, errors="replace")
        if ran.returncode != 0:
            return {"error": "blaze-build-script: " + ran.stderr.strip()}
        return json.loads(ran.stdout)

    cargo_env_file = os.path.join(blaze_dir, "cargo-env.json")
    runs = sorted(run_of, key=lambda i: run_hash(i))

    # What blaze reads of cargo's output of each run.
    def parse(i):
        run_dir, rec = recordings[run_of[i]]
        path = os.path.join(blaze_dir, run_hash(i) + "-parse.json")
        with open(path, "w") as f:
            json.dump(unit_json(i, rec["env"]["OUT_DIR"], [], []), f)
        output = printed(run_dir)
        if not os.path.isfile(output):
            return None
        return call("parse", path, cargo_env_file, output)

    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        parsed = dict(zip(runs, pool.map(parse, runs)))

    def paths_of(output):
        return [next(iter(p.values())) for p in output["library_paths"]]

    def blaze_run(i):
        unit = units[i]
        run_dir, rec = recordings[run_of[i]]
        dependencies = []
        plugin_search = []
        for d in unit["dependencies"]:
            dep = units[d["index"]]
            if dep["mode"] != "run-custom-build" or d["index"] not in run_of:
                continue
            out = (parsed.get(d["index"]) or {}).get("output") or {}
            dependencies.append({
                "name": packages[dep["pkg_id"]]["name"],
                "links": packages[dep["pkg_id"]].get("links"),
                "metadata": out.get("metadata", []),
            })
        compile_unit = next((d["index"] for d in unit["dependencies"]
                             if units[d["index"]]["mode"] == "build" and units[d["index"]]["pkg_id"] == unit["pkg_id"]
                             and "custom-build" in units[d["index"]]["target"]["kind"]), None)
        if compile_unit is not None:
            plugins = sorted(to_link(compile_unit), key=lambda r: (package_key(r), run_hash(r)))
            for r in plugins:
                out = (parsed.get(r) or {}).get("output") or {}
                plugin_search += paths_of(out) if out else []
        out_dir = os.path.join(blaze_dir, run_hash(i), "out")
        path = os.path.join(blaze_dir, run_hash(i) + "-run.json")
        with open(path, "w") as f:
            json.dump(unit_json(i, out_dir, dependencies, plugin_search), f)
        return compare(i, call("run", path, cargo_env_file), out_dir)

    def compare(i, ran, out_dir):
        run_dir, rec = recordings[run_of[i]]
        cargo_out = rec["env"]["OUT_DIR"]
        package = packages[units[i]["pkg_id"]]
        row = {"project": name, "package": package["name"], "version": package["version"], "run": run_hash(i)}
        if "env" not in ran:
            row.update(env_equal=False, parse_equal=False, run_equal=False, error=ran.get("error"))
            return row
        blaze_env = {k: v.replace(out_dir, cargo_out) for k, v in ran["env"].items()}
        diff = {k: [rec["env"].get(k), blaze_env.get(k)] for k in sorted(set(rec["env"]) | set(blaze_env))
                if rec["env"].get(k) != blaze_env.get(k)}
        row["env_equal"] = not diff
        if diff:
            row["env_diff"] = diff
        message = executed.get(os.path.realpath(cargo_out))
        reading = parsed.get(i)
        if message is not None:
            got = (reading or {}).get("output")
            if got is None:
                row["parse_equal"] = False
                row["parse_diff"] = (reading or {}).get("error", "no output file")
            else:
                said = {"linked_libs": message["linked_libs"], "linked_paths": message["linked_paths"],
                        "cfgs": message["cfgs"], "env": [list(e) for e in message["env"]]}
                read = {"linked_libs": got["library_links"], "linked_paths": paths_of(got),
                        "cfgs": got["cfgs"], "env": [list(e) for e in got["env"]]}
                row["parse_equal"] = said == read
                if said != read:
                    row["parse_diff"] = {k: [said[k], read[k]] for k in said if said[k] != read[k]}
        cargo_ok = message is not None
        blaze_ok = ran.get("output") is not None
        if cargo_ok != blaze_ok:
            row["run_equal"] = False
            row["run_note"] = f"cargo {'succeeded' if cargo_ok else 'failed'}, blaze " \
                              f"{'succeeded' if blaze_ok else 'failed'}: {ran.get('error') or ''}"[:2000]
        elif cargo_ok:
            # What the run comes to, not its bytes: cc prints the variables it looked up as
            # often as its threads happened to look.
            with open(printed(run_dir), errors="replace") as f:
                cargo_stdout = f.read()
            blaze_stdout = ran["stdout"].replace(out_dir, cargo_out)
            row["stdout_equal"] = cargo_stdout == blaze_stdout
            cargo_read = (reading or {}).get("output")
            blaze_read = json.loads(json.dumps(ran["output"]).replace(json.dumps(out_dir)[1:-1], json.dumps(cargo_out)[1:-1]))
            row["run_equal"] = cargo_read == blaze_read
            if not row["run_equal"]:
                row["run_note"] = {k: [(cargo_read or {}).get(k), v] for k, v in blaze_read.items()
                                   if (cargo_read or {}).get(k) != v}
        else:
            row["run_equal"] = True
        return row

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs // 8)) as pool:
        rows = list(pool.map(blaze_run, runs))
    unmatched = len(recordings) - len(run_of)
    if clean:
        shutil.rmtree(target_dir, ignore_errors=True)
        shutil.rmtree(blaze_dir, ignore_errors=True)
    return rows, unmatched, checked.returncode


def main():
    if os.environ.get(WRAP) and len(sys.argv) > 1 and not sys.argv[1].startswith("-"):
        wrap(sys.argv[1:])
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("names", nargs="*")
    parser.add_argument("--work", default=os.path.join(HERE, "vendor", "scripts"))
    parser.add_argument("--json")
    parser.add_argument("--clean", action="store_true", help="remove each project's builds once compared")
    parser.add_argument("--jobs", type=int, default=len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity")
                        else os.cpu_count())
    a = parser.parse_args()
    specs = corpus()
    names = a.names or list(specs)
    t = tools()
    blaze = blaze_build_script()
    work = os.path.abspath(a.work)
    all_rows = []
    totals = [0] * 7
    for name in names:
        rows, unmatched, status = project(name, specs[name], work, t, blaze, a.jobs, a.clean)
        all_rows += rows
        env_eq = sum(r["env_equal"] for r in rows)
        parse_rows = [r for r in rows if "parse_equal" in r]
        parse_eq = sum(r["parse_equal"] for r in parse_rows)
        run_eq = sum(r["run_equal"] for r in rows)
        stdout_eq = sum(r.get("stdout_equal", False) for r in rows)
        stdout_rows = sum("stdout_equal" in r for r in rows)
        for k, v in enumerate((len(rows), env_eq, len(parse_rows), parse_eq, run_eq, stdout_eq, stdout_rows)):
            totals[k] += v
        note = f", {unmatched} runs not found in the unit graph" if unmatched else ""
        note += ", cargo failed" if status else ""
        print(f"{name}: {len(rows)} scripts, env {env_eq}/{len(rows)}, parse {parse_eq}/{len(parse_rows)}, "
              f"run {run_eq}/{len(rows)}, stdout {stdout_eq}/{stdout_rows}{note}", flush=True)
        for r in rows:
            if not (r["env_equal"] and r.get("parse_equal", True) and r["run_equal"]):
                print("  " + json.dumps(r)[:600], flush=True)
    n, env_eq, np, parse_eq, run_eq, stdout_eq, stdout_rows = totals
    print(f"total: {n} scripts, env {env_eq}/{n}, parse {parse_eq}/{np}, run {run_eq}/{n}, "
          f"stdout {stdout_eq}/{stdout_rows}")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(all_rows, f, indent=1)


if __name__ == "__main__":
    main()
