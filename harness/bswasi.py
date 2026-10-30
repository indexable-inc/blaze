#!/usr/bin/env python3
"""bswasi.py: which build scripts of the bench corpus run unchanged under WASI (docs/cargo.md,
"Build scripts under WASI"), and why each other one does not.

usage: bswasi.py record [PROJECT...] --ra-jit BIN [--work DIR] [--out DIR] [--jobs N]
       bswasi.py report --out DIR

`record` fetches each project of harness/bench.py at its pinned revision (all of them when none
is named) and runs `cargo check --compile-time-deps --workspace --all-targets` through ra-jit's
wrapper (RUSTC_WRAPPER=ra-jit-rustc beside BIN), as harness/bscache.py does, with an empty store
of its own, but with RA_JIT_BUILD_SCRIPTS=wasi-compare and RA_JIT_SCRIPTS_BY_RUSTC=1: every
build script is compiled by rustc for the host and by ra-jit for wasm32-wasip1, and each run
runs the module under Wasmtime first, setting aside what it printed and wrote (`ra-jit-wasi/`
beside OUT_DIR), then the native executable in the sandbox, for cargo. BIN is built with the
feature `wasi`. RA_JIT_ONLY_ASKS is not set: the module's description needs the records the
wrapper keeps of the libraries rustc compiles. Each project's runs, each compared, are collected
into `<out>/<project>.json`, and the project's checkout, target directory and store are removed
(BSWASI_KEEP=1 keeps them).

`report` puts every run in one category and writes `<out>/README.md` and `<out>/runs.json`:

  works       the module exited 0 and printed what the native run printed, byte for byte,
              and left the same files in OUT_DIR, by path and bytes
  host cfg    it did otherwise and the script's own source (build.rs, build/) tests the
              platform it runs on: `cfg!(...)`/`#[cfg(...)]` of target_os, target_family,
              unix, windows, target_arch, target_pointer_width, target_env or target_vendor,
              or `std::env::consts`
  threads     it panicked spawning a thread
  other: ...  anything else, by what it was (the categories below are named by the report)
  native failed  the native run failed too: not counted against WASI
"""
import collections, hashlib, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from bscache import RAN, projects, run_dirs  # noqa: E402
from resolve import fetch_git, option  # noqa: E402

ASIDE = "ra-jit-wasi"
TOOLS = "ra-jit-wasi-tools.jsonl"
HOST_CFG = re.compile(
    r"cfg!\s*\(|#\s*\[\s*cfg\s*\(|cfg_attr\s*\(|env::consts|std::env::consts"
)
HOST_CFG_KEY = re.compile(
    r"\b(target_os|target_family|unix|windows|target_arch|target_pointer_width|target_env|target_vendor|target_endian)\b|consts::(OS|ARCH|FAMILY)"
)


def record(name, repo, rev, work, ra_jit, jobs):
    source = os.path.join(work, "src", name)
    fetch_git(repo, rev, source)
    target = os.path.join(work, "target", name)
    store = os.path.join(work, "store", name)
    for dir in (target, store):
        shutil.rmtree(dir, ignore_errors=True)
    env = dict(os.environ)
    for key in list(env):
        if key.startswith(("CARGO_", "RA_JIT_", "RUSTC_")) and key != "CARGO_HOME":
            del env[key]
    env.update(
        RUSTC_WRAPPER=os.path.join(os.path.dirname(ra_jit), "ra-jit-rustc"),
        RA_JIT_PROC_MACRO_SERVER="0",
        RA_JIT_SCRIPTS_BY_RUSTC="1",
        RA_JIT_BUILD_SCRIPTS="wasi-compare",
        RA_JIT_CACHE=store,
        CARGO_TARGET_DIR=target,
        CARGO_BUILD_JOBS=str(jobs),
        CARGO_UNSTABLE_CHECKSUM_FRESHNESS="false",
        CARGO_HOME=os.path.join(work, "cargo-home"),
        RUST_BACKTRACE="0",
    )
    # Cargo of a Nix shell gives scripts `RUSTC=rustc`, a name, which ra-jit's rustc for them
    # (as_rustc.rs) does not take for the toolchain's: a path is.
    rustc = shutil.which("rustc", path=env.get("PATH"))
    if rustc:
        env["RUSTC"] = rustc
    command = ["cargo", "check", "-Zunstable-options", "--compile-time-deps", "--workspace", "--all-targets", "--keep-going"]
    proc = subprocess.run(command, cwd=source, env=env, capture_output=True, text=True, errors="replace")
    failed = sorted(set(re.findall(r"failed to run custom build command for `([^`]+)`", proc.stderr)))
    runs = [compared(dir) for dir in run_dirs(target)]
    # An ix VM's disk is held in its memory: a corpus of target directories kills it.
    if not os.environ.get("BSWASI_KEEP"):
        for dir in (target, store, source):
            shutil.rmtree(dir, ignore_errors=True)
    return dict(
        project=name,
        rev=rev,
        status=proc.returncode,
        failed=failed,
        stderr_tail=proc.stderr[-4000:] if proc.returncode else "",
        source=source,
        target=target,
        runs=sorted(runs, key=lambda it: (it["package"] or "", it["out_dir"] or "")),
    )


def files_of(dir):
    """Each file below `dir`, by its path there, with the digest of its bytes."""
    files = {}
    for root, _, names in os.walk(dir):
        for name in names:
            path = os.path.join(root, name)
            if os.path.islink(path):
                files[os.path.relpath(path, dir)] = "link " + os.readlink(path)
            else:
                files[os.path.relpath(path, dir)] = hashlib.sha256(open(path, "rb").read()).hexdigest()
    return files


def read(path, mode="r"):
    try:
        with open(path, mode, **({} if "b" in mode else dict(errors="replace"))) as f:
            return f.read()
    except OSError:
        return None


def compared(dir):
    """What the run in `dir` did under WASI beside what the native run did."""
    ran = json.load(open(os.path.join(dir, RAN)))
    var = dict(ran["env"])
    out_dir = var.get("OUT_DIR")
    aside = os.path.join(dir, ASIDE)
    result = json.loads(read(os.path.join(aside, "result.json")) or "null")
    native_stdout = read(os.path.join(dir, "run", "stdout"), "rb")
    if native_stdout is None:
        native_stdout = read(os.path.join(dir, "output"), "rb")
    wasi_stdout = read(os.path.join(aside, "stdout"), "rb")
    native_files = files_of(out_dir) if out_dir and os.path.isdir(out_dir) else None
    wasi_files = files_of(os.path.join(aside, "out")) if os.path.isdir(os.path.join(aside, "out")) else None
    native_lines = (native_stdout or b"").decode(errors="replace").splitlines()
    wasi_lines = (wasi_stdout or b"").decode(errors="replace").splitlines()
    return dict(
        package=var.get("CARGO_PKG_NAME"),
        version=var.get("CARGO_PKG_VERSION"),
        manifest_dir=var.get("CARGO_MANIFEST_DIR"),
        out_dir=out_dir,
        real=ran["real"],
        result=result,
        wasi_stderr=(read(os.path.join(aside, "stderr")) or "")[-3000:],
        native_stdout_known=native_stdout is not None,
        stdout_same=native_stdout is not None and native_stdout == wasi_stdout,
        files_same=native_files is not None and native_files == wasi_files,
        only_native=[l for l in native_lines if l not in set(wasi_lines)][:20],
        only_wasi=[l for l in wasi_lines if l not in set(native_lines)][:20],
        files_only_native=sorted(set(native_files or {}) - set(wasi_files or {}))[:20],
        files_only_wasi=sorted(set(wasi_files or {}) - set(native_files or {}))[:20],
        files_differ=sorted(k for k in set(native_files or {}) & set(wasi_files or {}) if native_files[k] != wasi_files[k])[:20],
        host_cfg=host_cfg(var.get("CARGO_MANIFEST_DIR")),
    )


def host_cfg(package_dir):
    """The lines of the package's own build script source that test the platform it runs on."""
    if not package_dir:
        return []
    paths = [os.path.join(package_dir, "build.rs")]
    manifest = read(os.path.join(package_dir, "Cargo.toml")) or ""
    named = re.search(r'^\s*build\s*=\s*"([^"]+)"', manifest, re.M)
    if named:
        paths.append(os.path.join(package_dir, named.group(1)))
    for dir in ("build",):
        for root, _, names in os.walk(os.path.join(package_dir, dir)):
            paths += [os.path.join(root, n) for n in names if n.endswith(".rs")]
    hits = []
    for path in sorted(set(paths)):
        for line in (read(path) or "").splitlines():
            if HOST_CFG.search(line) and HOST_CFG_KEY.search(line):
                hits.append(f"{os.path.relpath(path, package_dir)}: {line.strip()[:160]}")
    return hits[:10]


def panic_of(stderr):
    m = re.search(r"panicked at [^\n]*:\n([^\n]*)", stderr)
    return m.group(1).strip() if m else None


def category(run, native_failed):
    """The category of `run` and what tells it."""
    result = run["result"]
    if result is None:
        return "other: not run under WASI", "no ra-jit-wasi/result.json"
    if "error" in result:
        error = result["error"]
        if "did not build" in error:
            reason = re.sub(r"^.*?: ", "", error.split("\n")[0], count=1)
            return "other: not built for wasm32-wasip1", reason[:300]
        return "other: not run under WASI", error[:300]
    refused = sorted({t["program"] for t in result["tools"] if t.get("refused")})
    stderr = run["wasi_stderr"]
    panic = panic_of(stderr) or ""
    same = result["status"] == 0 and run["stdout_same"] and run["files_same"]
    if same:
        return "works", ("ran " + ", ".join(sorted({t["program"].rsplit("/", 1)[-1] for t in result["tools"]}))) if result["tools"] else ""
    if native_failed:
        return "native failed", ""
    if "failed to spawn thread" in stderr or "thread::spawn" in panic:
        return "threads", panic
    if run["host_cfg"]:
        return "host cfg", "; ".join(run["host_cfg"][:3])
    if refused:
        return "other: runs a program a script under WASI may not", ", ".join(refused)
    if "output path is not a writable directory" in stderr:
        return "other: WASI has no permission bits (autocfg sees OUT_DIR read-only)", panic
    if re.search(r"Capabilities insufficient|No such file or directory|NotFound|os error 76|os error 44", stderr):
        return "other: reads outside its package and OUT_DIR", panic or stderr.strip().splitlines()[-1][:200]
    if re.search(r"not supported on this platform|Unsupported|unsupported", stderr):
        return "other: an operation WASI std does not support", panic or stderr.strip().splitlines()[-1][:200]
    unset = sorted(unset_variables(run))
    if unset:
        return "other: reads a variable cargo does not set for scripts, not given under WASI", ", ".join(unset)
    if result["status"] != 0:
        line = panic or (result.get("trap") or "").split("\n")[0] or stderr.strip()[-200:]
        return "other: failed", line[:300]
    return "other: printed or wrote something else", diff_of(run)


DOCUMENTED = re.compile(
    r"^(CARGO|CARGO_MANIFEST_DIR|CARGO_MANIFEST_PATH|CARGO_MANIFEST_LINKS|CARGO_MAKEFLAGS|CARGO_ENCODED_RUSTFLAGS|OUT_DIR|TARGET|HOST|NUM_JOBS|OPT_LEVEL|DEBUG|PROFILE|RUSTC|RUSTDOC|RUSTC_LINKER|RUSTC_WRAPPER|RUSTC_WORKSPACE_WRAPPER|CARGO_FEATURE_\w+|CARGO_CFG_\w+|CARGO_PKG_\w+|DEP_\w+)$"
)


def unset_variables(run):
    """The variables outside cargo's documented ones that the native run printed as set (as
    cc-rs prints `NAME = Some(..)`) and the run under WASI printed as unset."""
    native = {m.group(1) for l in run["only_native"] for m in [re.match(r"^(\w+) = Some\(", l)] if m}
    wasi = {m.group(1) for l in run["only_wasi"] for m in [re.match(r"^(\w+) = None$", l)] if m}
    return {name for name in native & wasi if not DOCUMENTED.match(name)}


def diff_of(run):
    parts = []
    if not run["stdout_same"]:
        parts.append("stdout native-only " + json.dumps(run["only_native"][:3]) + " wasi-only " + json.dumps(run["only_wasi"][:3]))
    if not run["files_same"]:
        parts.append(
            "files native-only %s wasi-only %s differ %s"
            % (run["files_only_native"][:3], run["files_only_wasi"][:3], run["files_differ"][:3])
        )
    return "; ".join(parts)


def report(out_dir):
    projects_done = sorted(f[:-5] for f in os.listdir(out_dir) if f.endswith(".json") and f != "runs.json")
    rows, runs_out = [], []
    by_category = collections.defaultdict(list)
    for name in projects_done:
        project = json.load(open(os.path.join(out_dir, f"{name}.json")))
        failed = {re.match(r"\S+ v\S+", f).group(0) for f in project["failed"] if re.match(r"\S+ v\S+", f)}
        counts = collections.Counter()
        for run in project["runs"]:
            crate = f"{run['package']} v{run['version']}"
            cat, why = category(run, crate in failed)
            counts[cat.split(":")[0] if cat.startswith("other") else cat] += 1
            by_category[cat].append((crate, name, why))
            runs_out.append(dict(project=name, crate=crate, category=cat, why=why, out_dir=run["out_dir"]))
        rows.append((name, project["status"], len(project["runs"]), counts))
    json.dump(runs_out, open(os.path.join(out_dir, "runs.json"), "w"), indent=1)
    cols = ["works", "host cfg", "threads", "other", "native failed"]
    text = [
        "# Build scripts of the bench corpus under WASI",
        "",
        "Made by `harness/bswasi.py record` then `report` (see its docstring for how a run is",
        "put in a category). One row per project; each run of a build script counts once.",
        "",
        "| project | cargo | runs | " + " | ".join(cols) + " |",
        "|---|---|---|" + "---|" * len(cols),
    ]
    total = collections.Counter()
    for name, status, n, counts in rows:
        total.update(counts)
        total["runs"] += n
        text.append(f"| {name} | {status} | {n} | " + " | ".join(str(counts[c]) for c in cols) + " |")
    text.append(f"| total | | {total['runs']} | " + " | ".join(str(total[c]) for c in cols) + " |")
    order = ["works", "host cfg", "threads"] + sorted(c for c in by_category if c.startswith("other")) + ["native failed"]
    for cat in order:
        if cat not in by_category:
            continue
        entries = by_category[cat]
        crates = collections.defaultdict(set)
        whys = {}
        for crate, project, why in entries:
            crates[crate].add(project)
            whys.setdefault(crate, why)
        text += ["", f"## {cat} ({len(entries)} runs, {len(crates)} crates)", ""]
        for crate in sorted(crates):
            why = whys[crate]
            line = f"- `{crate}` ({', '.join(sorted(crates[crate]))})"
            if why and cat != "works":
                line += ": " + why.replace("\n", " ").replace("|", "\\|")[:400]
            text.append(line)
    open(os.path.join(out_dir, "README.md"), "w").write("\n".join(text) + "\n")
    print("\n".join(text[: 8 + len(rows) + 1]))


def main():
    argv = sys.argv[1:]
    if not argv or argv[0] not in ("record", "report"):
        sys.exit(__doc__)
    mode = argv.pop(0)
    out_dir = option(argv, "--out", os.path.join(HERE, "..", "bench", "results", "bswasi"))
    os.makedirs(out_dir, exist_ok=True)
    if mode == "report":
        return report(out_dir)
    ra_jit = os.path.abspath(option(argv, "--ra-jit") or sys.exit("--ra-jit BIN"))
    work = option(argv, "--work", os.path.join(os.environ.get("TMPDIR", "/tmp"), "blaze-bswasi"))
    jobs = int(option(argv, "--jobs", "8"))
    corpus = projects()
    for name in argv or list(corpus):
        repo, rev = corpus[name]
        result = record(name, repo, rev, work, ra_jit, jobs)
        json.dump(result, open(os.path.join(out_dir, f"{name}.json"), "w"), indent=1)
        print(f"{name}: cargo {result['status']}, {len(result['runs'])} runs, failed {result['failed']}", flush=True)


if __name__ == "__main__":
    main()
