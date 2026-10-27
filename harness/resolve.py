#!/usr/bin/env python3
"""resolve.py: what blaze-manifest reads of a workspace against what cargo reads of it, over a
corpus. cargo is the oracle; blaze never runs it outside this harness.

usage: resolve.py [PART...] [--work DIR] [--jobs N] [--blaze BIN] [--blaze-metadata BIN] [--out FILE]
                  [--limit N]
       resolve.py DIR...                 compare the workspaces at DIR... only
       resolve.py --fixtures             write the expected JSON of the fixtures with cargo
       resolve.py --top-crates N         list the N most downloaded crates.io crates, newest
                                         stable versions, into the corpus

The parts of the corpus (harness/resolve/corpus.json) are
  bench       the projects of harness/bench.py at their pinned revisions
  crates      crates.io crates, each its `.crate` unpacked as a package of its own
  workspaces  workspace repositories at pinned commits
  config      `cargo -Zunstable-options config get` (RUSTC_BOOTSTRAP=1) against the `config`
              example, each of CONFIG_RUNS and its case's runs, on the trees of
              crates/blaze-manifest/tests/config-fixtures and the roots of the bench and
              workspace repositories: exit code, stdout and stderr compared as text
  profiles    the `profile` of every unit of `cargo <run> --unit-graph -Zunstable-options`
              against the `profiles` example's for that graph, each of PROFILE_RUNS and its
              case's runs, on the trees of crates/blaze-manifest/tests/profile-fixtures and
              the bench and workspace repositories: the profiles, the warnings about profiles
              and cargo's error when it refuses the profiles
  sysroot     `cargo metadata` of the toolchain's library/ (`--no-deps` and full, no platform and
              each target of harness/answers.py), asked as rust-analyzer's Sysroot asks it, against
              blaze-metadata's `metadata` example (`--no-deps`, `--locked`)
and every part runs when none is named. Each is fetched under `--work` (default
$TMPDIR/blaze-resolve) with a CARGO_HOME of its own there, then `cargo metadata --no-deps
--format-version 1` and the `metadata` example of blaze-manifest are run on it from its root.
Both outputs are normalized (the root's absolute path and CARGO_HOME replaced, so that runs on
different machines compare) and compared as text, key order included. A workspace is counted
equal, different, blaze-failed, cargo-failed or both-failed (both refused it, with cargo's error
text or not), and the first difference of each that is not equal is printed. `--out` writes every case and its class as
JSON; the counts are docs/resolve.md's.

`--fixtures` runs cargo on each fixture of crates/blaze-manifest/tests/fixtures, copied into a
directory of its own, from the directory its `fixture.json` names in `cwd` (its root if none),
and writes what it printed, normalized, as the fixture's `expected.json`, or `{"error": ...}`
with the error's lines when cargo refused it, and each config and profile tree's runs as its
`expected.json` (a profile tree's with the unit graph cargo printed and the host it ran on):
the crate's tests compare against those without cargo or the network. The cargo is the pinned
nightly from `harness/toolchain` unless CARGO says otherwise; the blaze-manifest example is built with it unless
`--blaze` names one.
"""
import collections, concurrent.futures, json, os, re, shutil, subprocess, sys, tarfile, tempfile, threading, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CORPUS = os.path.join(HERE, "resolve", "corpus.json")
FIXTURES = os.path.join(ROOT, "crates", "blaze-manifest", "tests", "fixtures")
CONFIG_FIXTURES = os.path.join(ROOT, "crates", "blaze-manifest", "tests", "config-fixtures")
PROFILE_FIXTURES = os.path.join(ROOT, "crates", "blaze-manifest", "tests", "profile-fixtures")
PIN = open(os.path.join(HERE, "toolchain")).read().strip()
CARGO = os.environ.get("CARGO_ORACLE", f"cargo +{PIN}").split()
USER_AGENT = "blaze resolve.py (https://github.com/indexable-inc/blaze)"
PARTS = ["bench", "crates", "workspaces", "config", "profiles", "sysroot"]
# The `config get` arguments every config case runs, before those of its case.json.
CONFIG_RUNS = [[], ["--show-origin"], ["--merged", "no"], ["--merged", "no", "--show-origin"], ["--format", "json"], ["--format", "json-value"], ["--format", "json", "--show-origin"]]
# The commands every profiles case runs, before those of its case.json; `--target` names a
# triple that is not the host's (see profile_runs).
PROFILE_RUNS = [["build"], ["build", "--release"], ["test", "--no-run"], ["bench", "--no-run"], ["check"], ["build", "--target", "$OTHER"]]
BENCH_PROJECT = re.compile(r'^    "([\w.-]+)": dict\(\s*repo="([^"]+)",[^\n]*?rev="([0-9a-f]{40})"', re.M)
# A repository two parts share is fetched by one of them at a time.
FETCH_LOCKS = collections.defaultdict(threading.Lock)
CLASSES = ["equal", "different", "blaze-failed", "cargo-failed", "both-failed"]


def option(argv, name, default=None):
    if name in argv:
        i = argv.index(name)
        value = argv[i + 1]
        del argv[i : i + 2]
        return value
    return default


def run(command, cwd, env):
    proc = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True, errors="replace")
    return proc.returncode, proc.stdout, proc.stderr


def normalize(text, root, cargo_home):
    """The output with the machine's paths replaced, parsed and printed again in its own order."""
    for path, name in ((os.path.realpath(root), "$ROOT"), (root, "$ROOT"), (cargo_home, "$CARGO_HOME")):
        text = text.replace(json.dumps(path)[1:-1], name).replace("file://" + path, "file://" + name)
    return json.dumps(json.loads(text), indent=1)


def first_difference(a, b, at="$"):
    if type(a) is not type(b):
        return f"{at}: {json.dumps(a)[:300]} != {json.dumps(b)[:300]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            missing = [k for k in b if k not in a]
            extra = [k for k in a if k not in b]
            if missing or extra:
                return f"{at}: blaze lacks {missing}, has extra {extra}"
            return f"{at}: keys in order {list(a)}, cargo's {list(b)}"
        for k in a:
            if (d := first_difference(a[k], b[k], f"{at}.{k}")) is not None:
                return d
        return None
    if isinstance(a, list):
        for i, (x, y) in enumerate(zip(a, b)):
            if (d := first_difference(x, y, f"{at}[{i}]")) is not None:
                return d
        if len(a) != len(b):
            return f"{at}: {len(a)} items, cargo {len(b)}"
        return None
    return None if a == b else f"{at}: {json.dumps(a)[:300]} != {json.dumps(b)[:300]}"


def first_line(stderr):
    lines = [l for l in stderr.splitlines() if l.startswith("error")]
    return (lines[0] if lines else stderr.strip().splitlines()[-1] if stderr.strip() else "")[:300]


def error_text(stderr):
    """The error cargo printed, from its first `error:` line on, with blank lines dropped."""
    lines = stderr.splitlines()
    start = next((i for i, l in enumerate(lines) if l.startswith("error")), len(lines))
    return "\n".join(l.rstrip() for l in lines[start:] if l.strip())


def clean_env(cargo_home):
    env = {k: v for k, v in os.environ.items() if not k.startswith("CARGO_") or k == "CARGO_ORACLE"}
    return {**env, "CARGO_HOME": cargo_home}


def compare(case, blaze, cargo_home):
    """Runs both on the workspace at case["dir"] and classifies the result."""
    root = case["dir"]
    env = clean_env(cargo_home)
    args = ["metadata", "--no-deps", "--format-version", "1", "--offline"]
    c_code, c_out, c_err = run(CARGO + args, root, env)
    b_code, b_out, b_err = run([blaze, *args[1:]], root, env)
    result = dict(name=case["name"], part=case["part"])
    if c_code and b_code:
        c_error, b_error = error_text(c_err), error_text(b_err)
        result.update(cls="both-failed", same_error=c_error == b_error)
        if c_error != b_error:
            result.update(cargo=c_error[:600], blaze=b_error[:600])
    elif c_code:
        result.update(cls="cargo-failed", cargo=first_line(c_err))
    elif b_code:
        result.update(cls="blaze-failed", blaze=first_line(b_err))
    else:
        c_json, b_json = normalize(c_out, root, cargo_home), normalize(b_out, root, cargo_home)
        if c_json == b_json:
            result.update(cls="equal")
        else:
            result.update(cls="different", difference=first_difference(json.loads(b_json), json.loads(c_json)) or "formatting")
    return result


def config_case(directory):
    """A config fixture's case.json, its defaults filled in."""
    path = os.path.join(directory, "case.json")
    case = json.load(open(path)) if os.path.exists(path) else {}
    return dict(cwd=case.get("cwd", ""), home=case.get("home"), env=case.get("env", {}), runs=CONFIG_RUNS + case.get("runs", []))


def config_runs(root, case, blaze, cargo_home):
    """Each run of `config get` on the tree at root by cargo and blaze, normalized: (args, cargo's, blaze's)."""
    home = os.path.join(root, case["home"]) if case["home"] else cargo_home
    env = {k: v for k, v in clean_env(home).items() if k != "CARGO_ORACLE"}
    env.update({k: v.replace("$ROOT", root) for k, v in case["env"].items()})
    env["RUSTC_BOOTSTRAP"] = "1"
    cwd = os.path.join(root, case["cwd"])

    def norm(result):
        code, out, err = result
        for path, name in ((os.path.realpath(home), "$CARGO_HOME"), (home, "$CARGO_HOME"), (os.path.realpath(root), "$ROOT"), (root, "$ROOT")):
            out, err = out.replace(path, name), err.replace(path, name)
        return dict(code=code, stdout=out, stderr=err)

    for args in case["runs"]:
        cargo = norm(run(CARGO + ["-Zunstable-options", "config", "get", *args], cwd, env))
        yield args, cargo, norm(run([blaze, "config", "get", *args], cwd, env)) if blaze else None


def compare_config(case, blaze, cargo_home):
    """Runs `config get` by both on case["dir"], copied away from the trees above it for a fixture."""
    result = dict(name=case["name"], part=case["part"])
    work = tempfile.mkdtemp(prefix="blaze-config-")
    try:
        if case.get("fixture"):
            root = os.path.join(os.path.realpath(work), case["name"])
            shutil.copytree(case["dir"], root, ignore=shutil.ignore_patterns("case.json", "expected.json"))
            spec = config_case(case["dir"])
        else:
            root, spec = case["dir"], dict(cwd="", home=None, env={}, runs=CONFIG_RUNS)
        runs = list(config_runs(root, spec, blaze, cargo_home))
    finally:
        shutil.rmtree(work)
    for args, c, b in runs:
        if c != b:
            stream = next(k for k in ("code", "stdout", "stderr") if c[k] != b[k])
            if stream == "code":
                detail = f"exit {b['code']}, cargo's {c['code']}"
            else:
                cl, bl = c[stream].splitlines(), b[stream].splitlines()
                i = next((i for i, (x, y) in enumerate(zip(cl, bl)) if x != y), min(len(cl), len(bl)))
                detail = f"{stream} line {i + 1}: {json.dumps(bl[i] if i < len(bl) else None)}, cargo's {json.dumps(cl[i] if i < len(cl) else None)}"
            result.update(cls="different", difference=f"config get {' '.join(args)}: {detail}")
            return result
    result.update(cls="equal", runs=len(runs))
    return result


def rustc_host():
    rustc = ["rustc", CARGO[1]] if len(CARGO) > 1 and CARGO[1].startswith("+") else ["rustc"]
    out = subprocess.run(rustc + ["-vV"], capture_output=True, text=True, check=True).stdout
    return next(l.split(": ", 1)[1] for l in out.splitlines() if l.startswith("host: "))


def profile_case(directory):
    """A profile fixture's case.json, its defaults filled in; a repository's are the defaults."""
    path = directory and os.path.join(directory, "case.json")
    case = json.load(open(path)) if path and os.path.exists(path) else {}
    return dict(cwd=case.get("cwd", ""), runs=[dict(args=a) for a in PROFILE_RUNS] + case.get("runs", []))


def profile_warnings(stderr):
    """The warnings about profiles, sorted: cargo prints those of manifests in `HashMap` order."""
    message = lambda l: re.sub(r"^warning: \S*Cargo\.toml: ", "", l)
    return sorted(l for l in stderr.splitlines() if l.startswith("warning: ") and "profile" in message(l) and "(manifest) generated" not in l)


def reduce_graph(graph):
    """The parts of a unit graph the profiles example reads, and the profiles to compare."""
    keep = lambda u: dict(pkg_id=u["pkg_id"], target=dict(kind=u["target"]["kind"], name=u["target"]["name"]), profile=u["profile"], platform=u["platform"], mode=u["mode"], features=u["features"], dependencies=[dict(index=d["index"]) for d in u["dependencies"]])
    return dict(units=[keep(u) for u in graph["units"]], roots=graph["roots"])


def profile_runs(root, case, blaze, cargo_home, host):
    """Each run on the tree at root: (run, cargo's, blaze's), both as {code, profiles or error, warnings}."""
    other = "x86_64-unknown-linux-gnu" if "-apple-" in host else "aarch64-apple-darwin"
    base = {k: v for k, v in clean_env(cargo_home).items() if k != "CARGO_ORACLE"}
    base["RUSTC_BOOTSTRAP"] = "1"
    cwd = os.path.join(root, case["cwd"])

    def norm(text):
        for path, name in ((os.path.realpath(cargo_home), "$CARGO_HOME"), (cargo_home, "$CARGO_HOME"), (os.path.realpath(root), "$ROOT"), (root, "$ROOT")):
            text = text.replace(path, name)
        return text

    for spec in case["runs"]:
        args = [a.replace("$OTHER", other) for a in spec["args"]]
        env = dict(base, **{k: v.replace("$ROOT", root) for k, v in spec.get("env", {}).items()})
        code, out, err = run(CARGO + args + ["--unit-graph", "-Zunstable-options"], cwd, env)
        cargo = dict(code=code, warnings=norm("\n".join(profile_warnings(err))).splitlines())
        if code:
            cargo["error"] = norm(error_text(err))
        else:
            graph = json.loads(out)
            cargo["profiles"] = [u["profile"] for u in graph["units"]]
            cargo["graph"] = json.loads(norm(json.dumps(reduce_graph(graph))))
        mine = None
        if blaze:
            proc = subprocess.run([blaze, "--host", host, *args], cwd=cwd, env=env, input=out if not code else "", capture_output=True, text=True)
            mine = dict(code=proc.returncode, warnings=norm("\n".join(profile_warnings(proc.stderr))).splitlines())
            if proc.returncode:
                mine["error"] = norm(error_text(proc.stderr))
            else:
                mine["profiles"] = json.loads(proc.stdout)
        yield dict(spec, args=args), cargo, mine


def compare_profiles(case, blaze, cargo_home, host):
    """Runs each profiles run on case["dir"], copied away from the trees above it for a fixture."""
    result = dict(name=case["name"], part=case["part"])
    work = tempfile.mkdtemp(prefix="blaze-profiles-")
    try:
        if case.get("fixture"):
            root = os.path.join(os.path.realpath(work), case["name"])
            shutil.copytree(case["dir"], root, ignore=shutil.ignore_patterns("case.json", "expected.json"))
            spec = profile_case(case["dir"])
        else:
            root, spec = case["dir"], profile_case(None)
        runs = list(profile_runs(root, spec, blaze, cargo_home, host))
    finally:
        shutil.rmtree(work)
    units = skipped = 0
    for run_spec, c, b in runs:
        what = " ".join(run_spec["args"])
        if c["code"] and b["code"] and b["error"].startswith("error: cargo's unit graph"):
            # cargo refused the workspace past its profiles, which blaze read.
            skipped += 1
            continue
        if c["code"] and b["code"]:
            if c["error"] != b["error"]:
                result.update(cls="different", difference=f"{what}: error {json.dumps(b['error'][:300])}, cargo's {json.dumps(c['error'][:300])}")
                return result
            continue
        if c["code"] or b["code"]:
            result.update(cls="cargo-failed" if c["code"] else "blaze-failed", difference=f"{what}: {(c.get('error') or b.get('error'))[:300]}")
            return result
        for i, (x, y) in enumerate(zip(b["profiles"], c["profiles"])):
            if x != y:
                unit = c["graph"]["units"][i]
                where = f"{unit['pkg_id'].split('#')[-1]} {unit['target']['name']} {unit['mode']}"
                result.update(cls="different", difference=f"{what}: unit {i} ({where}): {first_difference(x, y)}")
                return result
        if len(b["profiles"]) != len(c["profiles"]):
            result.update(cls="different", difference=f"{what}: {len(b['profiles'])} profiles, cargo {len(c['profiles'])}")
            return result
        if b["warnings"] != c["warnings"]:
            result.update(cls="different", difference=f"{what}: warnings {b['warnings']}, cargo's {c['warnings']}")
            return result
        units += len(c["profiles"])
    if skipped == len(runs):
        result.update(cls="cargo-failed", cargo=first_line(runs[0][1]["error"]))
    else:
        result.update(cls="equal", runs=len(runs) - skipped, units=units)
    return result


def sysroot_library():
    rustc = ["rustc", CARGO[1]] if len(CARGO) > 1 and CARGO[1].startswith("+") else ["rustc"]
    sysroot = subprocess.run(rustc + ["--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip()
    return rustc, os.path.join(sysroot, "lib", "rustlib", "src", "rust", "library")


def sysroot_cases():
    """A case for each way rust-analyzer asks for the library: no platform and each target ra-jit
    has the answers of, `--no-deps` and full."""
    sys.path.insert(0, HERE)
    from answers import TARGETS
    return [dict(name=f"{'no-deps' if no_deps else 'full'}/{target or 'any'}", part="sysroot", dir=None, fetch=lambda: None, no_deps=no_deps, target=target)
            for target in [None, *TARGETS] for no_deps in (True, False)]


def build_metadata_example():
    command = CARGO + ["build", "--release", "-p", "blaze-metadata", "--example", "metadata", "--message-format", "json"]
    proc = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable") and message["target"]["name"] == "metadata":
            return message["executable"]
    raise SystemExit("cargo built no metadata example of blaze-metadata")


def compare_sysroot(case, blaze, cargo_home):
    """cargo's and blaze's metadata of the toolchain's library, asked as rust-analyzer asks."""
    rustc, library = sysroot_library()
    result = dict(name=case["name"], part="sysroot")
    manifest = os.path.join(library, "Cargo.toml")
    env = {k: v for k, v in clean_env(cargo_home).items() if k != "CARGO_ORACLE"}
    env["__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS"] = "nightly"
    args = ["metadata", "--format-version", "1"] + (["--no-deps"] if case["no_deps"] else [])
    args += ["--features", "sysroot/backtrace", "--manifest-path", manifest]
    platform = ["--filter-platform", case["target"]] if case["target"] else []
    work = tempfile.mkdtemp(prefix="blaze-sysroot-")
    try:
        cargo_env = dict(env)
        cargo_args = args + platform
        if not case["no_deps"]:
            # rust-analyzer's copy of the lock file, so that the toolchain's is never written.
            copy = os.path.join(work, "Cargo.lock")
            shutil.copyfile(os.path.join(library, "Cargo.lock"), copy)
            cargo_env["CARGO_RESOLVER_LOCKFILE_PATH"] = copy
            cargo_args += ["-Zunstable-options"]
        c_code, c_out, c_err = run(CARGO + cargo_args, library, cargo_env)
        blaze_args = args[3:] + platform + (["--no-deps"] if case["no_deps"] else ["--locked"])
        if case["target"]:
            cfg = os.path.join(work, "cfg")
            with open(cfg, "w") as f:
                f.write(subprocess.run(rustc + ["--print", "cfg", "--target", case["target"]], capture_output=True, text=True, check=True).stdout)
            blaze_args += ["--cfg-file", cfg]
        b_code, b_out, b_err = run([blaze, *blaze_args], library, env)
    finally:
        shutil.rmtree(work)
    if c_code or b_code:
        cls = "both-failed" if c_code and b_code else "cargo-failed" if c_code else "blaze-failed"
        result.update(cls=cls, cargo=first_line(c_err) if c_code else "", blaze=b_err.strip()[-600:] if b_code else "")
        return result
    c_json, b_json = normalize(c_out, library, cargo_home), normalize(b_out, library, cargo_home)
    if c_json == b_json:
        result.update(cls="equal", packages=len(json.loads(c_json)["packages"]))
    else:
        result.update(cls="different", difference=first_difference(json.loads(b_json), json.loads(c_json)) or "formatting")
    return result


def fetch_git(repo, rev, dest):
    with FETCH_LOCKS[dest]:
        fetch_git_unlocked(repo, rev, dest)


def fetch_git_unlocked(repo, rev, dest):
    if os.path.exists(os.path.join(dest, ".fetched")):
        return
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    git = lambda *a: subprocess.run(["git", "-C", dest, *a], check=True, capture_output=True)
    git("init", "-q")
    git("fetch", "-q", "--depth", "1", f"https://github.com/{repo}", rev)
    git("-c", "advice.detachedHead=false", "checkout", "-q", "FETCH_HEAD")
    open(os.path.join(dest, ".fetched"), "w").close()


def fetch_crate(name, version, dest):
    with FETCH_LOCKS[dest]:
        fetch_crate_unlocked(name, version, dest)


def fetch_crate_unlocked(name, version, dest):
    if os.path.exists(os.path.join(dest, ".fetched")):
        return
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request) as response, tempfile.TemporaryFile() as file:
        shutil.copyfileobj(response, file)
        file.seek(0)
        with tarfile.open(fileobj=file, mode="r:gz") as archive:
            archive.extractall(dest, filter="data")
    open(os.path.join(dest, ".fetched"), "w").close()


def cases_of(part, corpus, work, limit):
    """The workspaces of a part, fetched: (name, directory) each."""
    cases = []
    if part == "sysroot":
        return sysroot_cases()
    if part in ("config", "profiles"):
        fixtures = CONFIG_FIXTURES if part == "config" else PROFILE_FIXTURES
        for name in sorted(os.listdir(fixtures)):
            cases.append(dict(name=name, part=part, dir=os.path.join(fixtures, name), fetch=lambda: None, fixture=True))
        for repos in ("bench", "workspaces"):
            cases += [dict(c, part=part, name=f"{repos}/{c['name']}") for c in cases_of(repos, corpus, work, limit)]
        return cases
    if part == "bench":
        # Read, not imported: bench.py imports what needs a built ra-jit.
        source = open(os.path.join(HERE, "bench.py")).read()
        entries = [dict(name=n, repo=r, rev=v) for n, r, v in BENCH_PROJECT.findall(source)]
    else:
        entries = corpus[part]
    for entry in entries[:limit]:
        if part == "crates":
            name, version = entry.split("@")
            dest = os.path.join(work, "crates", entry)
            fetch = lambda d=dest, n=name, v=version: fetch_crate(n, v, d)
            directory = os.path.join(dest, f"{name}-{version}")
            cases.append(dict(name=entry, part=part, dir=directory, fetch=fetch))
        else:
            dest = os.path.join(work, part, entry["name"])
            fetch = lambda d=dest, e=entry: fetch_git(e["repo"], e["rev"], d)
            cases.append(dict(name=entry["name"], part=part, dir=os.path.join(dest, entry.get("path", "")), fetch=fetch))
    return cases


def build_blaze():
    """The metadata, config and profiles examples, by name."""
    command = CARGO + ["build", "--release", "-p", "blaze-manifest", "--example", "metadata", "--example", "config", "--example", "profiles", "--message-format", "json"]
    proc = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    built = {}
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            built[message["target"]["name"]] = message["executable"]
    if set(built) != {"metadata", "config", "profiles"}:
        raise SystemExit(f"cargo built {sorted(built)}, not the metadata, config and profiles examples")
    return built


def write_fixtures():
    cargo_home = tempfile.mkdtemp(prefix="blaze-fixture-home-")
    for name in sorted(os.listdir(FIXTURES)):
        source = os.path.join(FIXTURES, name)
        work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-fixture-"))
        root = os.path.join(work, name)
        shutil.copytree(source, root, ignore=shutil.ignore_patterns("expected.json", "fixture.json"))
        manifest = json.load(open(os.path.join(source, "fixture.json"))) if os.path.exists(os.path.join(source, "fixture.json")) else {}
        cwd = os.path.join(root, manifest.get("cwd", ""))
        env = clean_env(cargo_home)
        code, out, err = run(CARGO + ["metadata", "--no-deps", "--format-version", "1"], cwd, env)
        if code:
            error = error_text(err).replace(os.path.realpath(root), "$ROOT").replace(root, "$ROOT")
            text = json.dumps({"error": error}, indent=1)
        else:
            text = normalize(out, root, cargo_home)
        with open(os.path.join(source, "expected.json"), "w") as f:
            f.write(text + "\n")
        print(f"{name}: {'error' if code else 'ok'}")
        shutil.rmtree(work)
    for name in sorted(os.listdir(CONFIG_FIXTURES)):
        source = os.path.join(CONFIG_FIXTURES, name)
        work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-fixture-"))
        root = os.path.join(work, name)
        shutil.copytree(source, root, ignore=shutil.ignore_patterns("case.json", "expected.json"))
        expected = [dict(args=args, **cargo) for args, cargo, _ in config_runs(root, config_case(source), None, cargo_home)]
        with open(os.path.join(source, "expected.json"), "w") as f:
            json.dump(expected, f, indent=1, ensure_ascii=False)
            f.write("\n")
        print(f"config/{name}: {len(expected)} runs")
        shutil.rmtree(work)
    host = rustc_host()
    for name in sorted(os.listdir(PROFILE_FIXTURES)):
        source = os.path.join(PROFILE_FIXTURES, name)
        work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-fixture-"))
        root = os.path.join(work, name)
        shutil.copytree(source, root, ignore=shutil.ignore_patterns("case.json", "expected.json"))
        runs = []
        for spec, cargo, _ in profile_runs(root, profile_case(source), None, cargo_home, host):
            cargo.pop("profiles", None)
            runs.append(dict(spec, **cargo))
        with open(os.path.join(source, "expected.json"), "w") as f:
            json.dump(dict(host=host, runs=runs), f, indent=1, ensure_ascii=False)
            f.write("\n")
        print(f"profiles/{name}: {len(runs)} runs")
        shutil.rmtree(work)
    shutil.rmtree(cargo_home)


def top_crates(count):
    names = []
    page = 1
    while len(names) < count:
        url = f"https://crates.io/api/v1/crates?sort=downloads&per_page=100&page={page}"
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        crates = json.load(urllib.request.urlopen(request))["crates"]
        if not crates:
            break
        names += [f"{c['id']}@{c['max_stable_version'] or c['newest_version']}" for c in crates]
        page += 1
    corpus = json.load(open(CORPUS))
    corpus["crates"] = names[:count]
    with open(CORPUS, "w") as f:
        json.dump(corpus, f, indent=1)
        f.write("\n")
    print(f"{len(corpus['crates'])} crates in {CORPUS}")


def main():
    argv = sys.argv[1:]
    if "--fixtures" in argv:
        return write_fixtures()
    if (count := option(argv, "--top-crates")) is not None:
        return top_crates(int(count))
    work = os.path.abspath(option(argv, "--work", os.path.join(tempfile.gettempdir(), "blaze-resolve")))
    jobs = int(option(argv, "--jobs", str(os.cpu_count() or 4)))
    out = option(argv, "--out")
    limit = int(option(argv, "--limit", "1000000"))
    blaze = option(argv, "--blaze")
    built = dict(metadata=blaze, config=os.path.join(os.path.dirname(blaze), "config"), profiles=os.path.join(os.path.dirname(blaze), "profiles")) if blaze else build_blaze()
    blaze_metadata = option(argv, "--blaze-metadata")
    if "sysroot" in (argv or PARTS):
        built["sysroot"] = blaze_metadata or build_metadata_example()
    host = rustc_host()
    cargo_home = os.path.join(work, "cargo-home")
    os.makedirs(cargo_home, exist_ok=True)

    dirs = [a for a in argv if a not in PARTS]
    if dirs:
        cases = [dict(name=d, part="dirs", dir=os.path.abspath(d), fetch=lambda: None) for d in dirs]
    else:
        corpus = json.load(open(CORPUS))
        cases = [c for part in (argv or PARTS) for c in cases_of(part, corpus, work, limit)]

    def one(case):
        try:
            case["fetch"]()
        except Exception as e:
            return dict(name=case["name"], part=case["part"], cls="fetch-failed", error=str(e)[:300])
        if case["part"] == "config":
            return compare_config(case, built["config"], cargo_home)
        if case["part"] == "sysroot":
            return compare_sysroot(case, built["sysroot"], cargo_home)
        if case["part"] == "profiles":
            return compare_profiles(case, built["profiles"], cargo_home, host)
        return compare(case, built["metadata"], cargo_home)

    results = []
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for result in pool.map(one, cases):
            results.append(result)
            if result["cls"] != "equal" and not result.get("same_error"):
                detail = result.get("difference") or "\n  ".join(f"{k}: {result[k]}".replace("\n", "\n    ") for k in ("blaze", "cargo", "error") if k in result)
                print(f"{result['part']}/{result['name']}: {result['cls']}: {detail}", flush=True)

    print()
    for part in dict.fromkeys(r["part"] for r in results):
        rows = [r for r in results if r["part"] == part]
        counts = {c: sum(r["cls"] == c for r in rows) for c in CLASSES + ["fetch-failed"]}
        same = sum(r.get("same_error", False) for r in rows)
        others = [f"{v} {k}" + (f" ({same} with cargo's error text)" if k == "both-failed" else "") for k, v in counts.items() if v and k != "equal"]
        print(f"{part}: {counts['equal']}/{len(rows)} equal" + "".join(", " + o for o in others))
    if out:
        with open(out, "w") as f:
            json.dump(results, f, indent=1)
            f.write("\n")
    failed = ("different", "blaze-failed", "cargo-failed")
    return 1 if any(r["cls"] in failed or r.get("same_error") is False for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
