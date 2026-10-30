#!/usr/bin/env python3
"""bscache.py: what the build scripts of the bench corpus reach, as the sandbox records it, and
what a hermetic sandbox would refuse them.

usage: bscache.py record [PROJECT...] --blaze BIN [--work DIR] [--out DIR] [--jobs N]
       bscache.py report --out DIR
       bscache.py directives [--work DIR] --out DIR
       bscache.py closure [PROJECT...] --blaze BIN --store DIR [--work DIR] [--out DIR] [--jobs N]
       bscache.py closure-report --out DIR

`record` fetches each project of harness/bench.py at its pinned revision (all of them when none
is named) and runs the step of a workspace load that builds and runs its build scripts and proc
macros, `cargo check --compile-time-deps --workspace --all-targets`, through blaze's wrapper
(RUSTC_WRAPPER=blaze-rustc beside BIN, BLAZE_ONLY_ASKS), with an empty store (BLAZE_CACHE of its own), so every
script runs, and runs in the sandbox (cargo/script_sandbox.rs) through `blaze-engine build-script`.
Proc macros are rustc's here (BLAZE_PROC_MACRO_SERVER=0): what is measured is the scripts'
runs. The sandbox leaves beside each script's OUT_DIR the record of what it reached
(`blaze-record`: R read, W write refused, N network refused, X record not whole) and how cargo
ran it (`blaze-ran.json`). Each project's are collected into `<out>/<project>.json`.

`report` classifies every line of every record and writes `<out>/README.md` and, every
would-be denial by package, `<out>/denied.json`:

  network     every N line: refused now, and by a hermetic sandbox
  writes      every W line: a write outside OUT_DIR, refused now
  executables an R line of a regular file with the executable bit and an ELF or `#!` start (the
              record does not tell a run from a look, so a script that looks for a program on
              PATH without running it counts too), outside the toolchain (its sysroot, blaze),
              the Nix closure of the C compiler (`cc`, `c++`, `ar`) and pkg-config, and the
              package (its own configure and scripts are inputs)
  reads       every other R line outside the package, its OUT_DIR, the toolchain and that
              closure; a dependency's OUT_DIR (what `links` gives a script by DEP_*) is listed
              apart, as a read a hermetic sandbox allows

`closure` runs the same step with every build script in its closure alone (BLAZE_SCRIPT_VIEW=on:
the package, OUT_DIR and the pinned nixpkgs toolchain and libraries in a mount namespace,
cargo/hermetic), `--keep-going`, against the store `--store` (which must hold the closure, or
BLAZE_REMOTE must bring it), and writes per project whether each script ran, came from the
store (its run's key is of what it is made of), or failed, with its note. Run it twice, the
second time on another machine whose store pulled the first's, for the hit rate across hosts.
`closure-report` writes `<out>/closure.md` of them.

`directives` checks what each run below `<work>/target` printed (`run/stdout`) against the rules
of cargo/directives.rs, and their first, stricter form, into `<out>/directives.md`.
"""
import collections, concurrent.futures, json, os, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from resolve import BENCH_PROJECT, fetch_git, option  # noqa: E402

RECORD = "blaze-record"
RAN = "blaze-ran.json"


def projects():
    source = open(os.path.join(HERE, "bench.py")).read()
    return {name: (repo, rev) for name, repo, rev in BENCH_PROJECT.findall(source)}


def run_dirs(target):
    """Each directory a script ran in below `target`: the parent of its OUT_DIR."""
    for root, dirs, files in os.walk(target):
        if RAN in files:
            yield root
            dirs[:] = [d for d in dirs if d == "out"]


def record(name, repo, rev, work, blaze, jobs):
    source = os.path.join(work, "src", name)
    fetch_git(repo, rev, source)
    target = os.path.join(work, "target", name)
    store = os.path.join(work, "store", name)
    for dir in (target, store):
        shutil.rmtree(dir, ignore_errors=True)
    env = dict(os.environ)
    for key in list(env):
        if key.startswith(("CARGO_", "BLAZE_", "RUSTC_")) and key != "CARGO_HOME":
            del env[key]
    env.update(
        RUSTC_WRAPPER=os.path.join(os.path.dirname(blaze), "blaze-rustc"),
        BLAZE_ONLY_ASKS="1",
        BLAZE_PROC_MACRO_SERVER="0",
        BLAZE_CACHE=store,
        CARGO_TARGET_DIR=target,
        CARGO_BUILD_JOBS=str(jobs),
        CARGO_UNSTABLE_CHECKSUM_FRESHNESS="false",
        CARGO_HOME=os.path.join(work, "cargo-home"),
        RUST_BACKTRACE="0",
    )
    command = ["cargo", "check", "-Zunstable-options", "--compile-time-deps", "--workspace", "--all-targets"]
    proc = subprocess.run(command, cwd=source, env=env, capture_output=True, text=True, errors="replace")
    failed = sorted(set(re.findall(r"failed to run custom build command for `([^`]+)`", proc.stderr)))
    scripts = []
    for dir in run_dirs(target):
        ran = json.load(open(os.path.join(dir, RAN)))
        var = dict(ran["env"])
        lines = None
        if os.path.exists(os.path.join(dir, RECORD)):
            lines = [line.split("\t", 2) for line in open(os.path.join(dir, RECORD)).read().splitlines()]
        scripts.append(
            dict(
                package=var.get("CARGO_PKG_NAME"),
                version=var.get("CARGO_PKG_VERSION"),
                manifest_dir=var.get("CARGO_MANIFEST_DIR"),
                out_dir=var.get("OUT_DIR"),
                real=ran["real"],
                rustc=var.get("RUSTC"),
                cargo_home=var.get("CARGO_HOME"),
                home=var.get("HOME"),
                env={k: var[k] for k in ("NIX_CFLAGS_COMPILE", "NIX_LDFLAGS", "CFLAGS", "LDFLAGS", "PKG_CONFIG_PATH") if k in var},
                record=[dict(reached=r, what=w) for r, _, w in lines] if lines is not None else None,
                executables=executables(lines or []),
            )
        )
    return dict(
        project=name,
        rev=rev,
        status=proc.returncode,
        failed=failed,
        stderr_tail=proc.stderr[-4000:] if proc.returncode else "",
        source=source,
        target=target,
        scripts=sorted(scripts, key=lambda it: (it["package"] or "", it["out_dir"] or "")),
    )


def executables(lines):
    """The paths of R lines that are programs: a regular file, executable, starting as one."""
    found = []
    for reached, _, what in lines:
        if reached != "R" or what.endswith(".so") or ".so." in os.path.basename(what):
            continue
        try:
            if not (os.path.isfile(what) and os.access(what, os.X_OK)):
                continue
            with open(what, "rb") as file:
                start = file.read(4)
        except OSError:
            continue
        if start == b"\x7fELF" or start[:2] == b"#!":
            found.append(what)
    return found


def closure(programs):
    """The Nix store paths `programs` run from, and everything they need."""
    roots = []
    for program in programs:
        found = shutil.which(program)
        if found:
            real = os.path.realpath(found)
            if real.startswith("/nix/store/"):
                roots.append("/".join(real.split("/")[:4]))
    if not roots:
        return []
    out = subprocess.run(["nix-store", "-qR", *roots], capture_output=True, text=True)
    return sorted(set(out.stdout.split()))


def under(path, dir):
    return bool(dir) and (path == dir or path.startswith(dir.rstrip("/") + "/"))


def flag_dirs(*values):
    """The directories `-I`, `-isystem`, `-idirafter`, `-L` and `-B` name in compiler flags."""
    dirs = []
    for value in values:
        words = (value or "").split()
        for at, word in enumerate(words):
            for flag in ("-isystem", "-idirafter", "-I", "-L", "-B"):
                if word == flag and at + 1 < len(words):
                    dirs.append(words[at + 1])
                elif word.startswith(flag) and len(word) > len(flag) and word[len(flag)] == "/":
                    dirs.append(word[len(flag):])
    return [os.path.normpath(it) for it in dirs]


def classify(project, allowed):
    """Each script's lines in classes: what a hermetic sandbox refuses and what it allows."""
    target = project["target"]
    source = project["source"]
    out = []
    for script in project["scripts"]:
        record = script["record"]
        if record is None:
            out.append(dict(script, classes=None))
            continue
        run_dir = os.path.dirname(script["out_dir"] or "")
        env = script.get("env")
        if env is None and os.path.exists(os.path.join(run_dir, RAN)):
            env = dict(json.load(open(os.path.join(run_dir, RAN)))["env"])
        env = env or {}
        toolchain = [allowed["sysroot"], allowed["blaze_dir"], os.path.dirname(script["real"]), run_dir]
        toolchain += allowed["closure"]
        # What the dev shell has the C compiler and pkg-config search.
        searched = flag_dirs(env.get("NIX_CFLAGS_COMPILE"), env.get("NIX_LDFLAGS"), env.get("CFLAGS"), env.get("LDFLAGS"))
        searched += [it for it in (env.get("PKG_CONFIG_PATH") or "").split(":") if it]
        roots = toolchain + searched + [script["out_dir"], script["manifest_dir"], target, source, script["cargo_home"]]
        exes = set(script["executables"])
        classes = collections.defaultdict(list)
        for line in record:
            reached, what = line["reached"], line["what"]
            name = os.path.basename(what)
            if reached == "N":
                classes["network"].append(what)
            elif reached == "W" and any(under(it, what) for it in (script["out_dir"],) if it):
                classes["allowed: making a directory above OUT_DIR, which is there"].append(what)
            elif reached == "W":
                classes["writes"].append(what)
            elif reached == "X":
                classes["incomplete"].append(what)
            elif any(under(what, it) for it in toolchain):
                classes["allowed: toolchain and C compiler"].append(what)
            elif what in exes and under(what, script["manifest_dir"]):
                classes["allowed: the package's own programs (configure, its scripts)"].append(what)
            elif what in exes and not under(what, source):
                classes["executables"].append(what)
            elif under(what, script["out_dir"]):
                classes["allowed: OUT_DIR"].append(what)
            elif under(what, script["manifest_dir"]):
                classes["allowed: package"].append(what)
            elif under(what, target) and re.search(r"/build/[^/]+(/[0-9a-f]+)?/out(/|$)", what):
                classes["allowed: a dependency's OUT_DIR"].append(what)
            elif any(under(what, it) for it in searched):
                classes["allowed: the C compiler's and pkg-config's search paths"].append(what)
            elif any(under(it, what) for it in roots if it):
                classes["allowed: a directory above an allowed one"].append(what)
            elif not os.path.lexists(what):
                classes["allowed: looked up, not there"].append(what)
            elif name.endswith(".so") or ".so." in name or what in ("/etc/ld-nix.so.preload", "/etc/ld.so.cache", "/etc/ld.so.preload"):
                classes["allowed: shared libraries and the loader's files"].append(what)
            elif name == ".cargo" or name in ("config", "config.toml") and "/.cargo/" in what or what == "/etc/_rjem_malloc.conf":
                classes["allowed: cargo's configuration, read by blaze as rustc"].append(what)
            elif under(what, target):
                classes["reads: target directory"].append(what)
            elif under(what, source):
                classes["reads: workspace outside the package"].append(what)
            elif under(what, script["cargo_home"]):
                classes["reads: cargo home (other packages)"].append(what)
            elif what.startswith("/nix/store/"):
                classes["reads: Nix store outside the C compiler"].append(what)
            elif what.startswith("/etc/"):
                classes["reads: /etc"].append(what)
            elif under(what, script["home"]):
                classes["reads: home"].append(what)
            elif what.startswith(("/proc/", "/sys/", "/dev/")):
                classes["allowed: kernel views"].append(what)
            else:
                classes["reads: elsewhere"].append(what)
        out.append(dict(script, classes=dict(classes)))
    return out


REFUSED = ["network", "writes", "executables"]


def refused(classes):
    return {k: v for k, v in classes.items() if k in REFUSED or k.startswith("reads:") or k == "incomplete"}


# Every would-be denial, by package: what `report` writes beside README.md.
DENIED = "denied.json"


def report(out_dir):
    rows, details, failures = [], [], []
    totals = collections.Counter()
    for file in sorted(os.listdir(out_dir)):
        if not file.endswith(".json") or file == DENIED:
            continue
        project = json.load(open(os.path.join(out_dir, file)))
        scripts = classify(project, project["allowed"])
        count = collections.Counter()
        for script in scripts:
            count["scripts"] += 1
            if script["classes"] is None:
                count["no record"] += 1
                continue
            denied = refused(script["classes"])
            count["clean"] += not denied
            for kind in REFUSED:
                count[kind] += kind in denied
            count["reads"] += any(k.startswith("reads:") for k in denied)
            if denied:
                details.append((project["project"], script, denied))
        count["failed"] = len(project["failed"])
        for package in project["failed"]:
            said = [line.strip() for line in project["stderr_tail"].splitlines()]
            said = [line for line in said if line.startswith(("note:", "Error", "thread", "error:")) or "panicked" in line]
            failures.append((project["project"], dict(package=package, said=said[-6:])))
        totals.update(count)
        rows.append((project["project"], project["status"], count))
    lines = [
        "# Build scripts of the bench corpus under the sandbox's record",
        "",
        "Made by `harness/bscache.py record` then `report` (see its docstring for how each line is",
        "classed). One row per project; a script counts once per class it has a line in.",
        "",
        "| project | cargo | scripts | failed | no record | clean | network | writes | executables | reads |",
        "|---|---|---|---|---|---|---|---|---|---|",
    ]
    for name, status, c in rows + [("total", "", totals)]:
        cells = [c[k] for k in ("scripts", "failed", "no record", "clean", "network", "writes", "executables", "reads")]
        lines.append(f"| {name} | {status} | " + " | ".join(map(str, cells)) + " |")
    lines += ["", "## Scripts that failed", ""]
    for name, file in sorted(failures):
        lines.append(f"### {name}: `{file['package']}`")
        lines += ["", "```", *file["said"], "```", ""]
    lines += ["", "## What each script would be refused", ""]
    by_package = collections.OrderedDict()
    for project, script, denied in details:
        key = f"{script['package']} {script['version']}"
        entry = by_package.setdefault(key, dict(projects=[], denied=collections.defaultdict(set)))
        entry["projects"].append(project)
        for kind, whats in denied.items():
            entry["denied"][kind].update(tidy(what, script) for what in whats)
    for key, entry in sorted(by_package.items()):
        lines.append(f"### `{key}` ({', '.join(sorted(set(entry['projects'])))})")
        lines.append("")
        for kind, whats in sorted(entry["denied"].items()):
            shown = sorted(whats)
            more = f" and {len(shown) - 12} more" if len(shown) > 12 else ""
            lines.append(f"- {kind}: " + ", ".join(f"`{it}`" for it in shown[:12]) + more)
        lines.append("")
    open(os.path.join(out_dir, "README.md"), "w").write("\n".join(lines))
    denied = {key: dict(projects=sorted(set(entry["projects"])),
                        denied={kind: dict(count=len(whats), first=sorted(whats)[:50])
                                for kind, whats in sorted(entry["denied"].items())})
              for key, entry in sorted(by_package.items())}
    json.dump(denied, open(os.path.join(out_dir, DENIED), "w"), indent=1, sort_keys=True)
    print("\n".join(lines[:8 + len(rows) + 1]))


def tidy(what, script):
    """`what` with the places of one machine named by what they are."""
    for place, name in ((script["manifest_dir"], "$PACKAGE"), (script["cargo_home"], "$CARGO_HOME"), (script["home"], "$HOME")):
        if place and what.startswith(place.rstrip("/") + "/"):
            return name + what[len(place.rstrip("/")):]
    return re.sub(r"^/nix/store/[0-9a-z]{32}-", "/nix/store/*-", what)


# What cargo/directives.rs checks, as it checks it; `strict` is the first form of each rule,
# before the corpus's counts set its edges.
RESERVED = ["rustc-flags", "rustc-link-lib", "rustc-link-search", "rustc-link-arg-cdylib",
            "rustc-cdylib-link-arg", "rustc-link-arg-bins", "rustc-link-arg-bin",
            "rustc-link-arg-tests", "rustc-link-arg-benches", "rustc-link-arg-examples",
            "rustc-link-arg", "rustc-cfg", "rustc-check-cfg", "rustc-env", "warning",
            "rerun-if-changed", "rerun-if-env-changed"]
SYSTEM_LIB_DIRS = ["/usr/lib", "/usr/lib64", "/usr/local/lib", "/lib", "/lib64", "/nix/store",
                   "/opt/homebrew/lib", "/opt/homebrew/opt", "/usr/local/opt",
                   "/Library/Developer/CommandLineTools/SDKs",
                   "/Applications/Xcode.app/Contents/Developer/Platforms"]
LINK_ARGS = ["-pthread", "-rdynamic", "-nostartfiles", "-Wl,--as-needed", "-Wl,--no-as-needed",
             "-Wl,--gc-sections", "-Wl,--export-dynamic", "-Wl,-z,relro", "-Wl,-z,now",
             "-Wl,-z,noexecstack", "-Wl,-z,nodelete", "-Wl,-undefined,dynamic_lookup",
             "-Wl,-dead_strip", "-Wl,-headerpad_max_install_names", "-ObjC", "-Wl,-ObjC"]
LINK_ARG_PREFIXES = ["-Wl,--export-dynamic-symbol=", "-Wl,-soname,", "-Wl,-install_name,@rpath/",
                     "-Wl,-exported_symbol,", "-Wl,--version-script="]
CARGO_GIVES = ["CARGO", "CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH", "CARGO_MANIFEST_LINKS",
               "CARGO_CRATE_NAME", "CARGO_BIN_NAME", "CARGO_PRIMARY_PACKAGE", "CARGO_TARGET_TMPDIR",
               "CARGO_RUSTC_CURRENT_DIR", "CARGO_ENCODED_RUSTFLAGS", "CARGO_MAKEFLAGS", "OUT_DIR",
               "RUSTC", "RUSTDOC", "RUSTFLAGS"]
CARGO_GIVES_PREFIXES = ["CARGO_PKG_", "CARGO_CFG_", "CARGO_BIN_EXE_", "CARGO_FEATURE_", "RUSTC_", "RUSTDOC_"]


def directive_refusals(stdout, package_dir, out_dir):
    """(rule, line) for each line of `stdout` a rule refuses: `strict-*` as first written, the
    others as cargo/directives.rs has them."""
    build_dir = os.path.dirname(os.path.dirname(out_dir))
    if os.path.basename(build_dir) != "build":
        build_dir = os.path.dirname(build_dir)
    def may_search(d, system=True):
        if not d.startswith("/"):
            return ".." not in d.split("/")
        d = os.path.normpath(d)
        return any(under(d, it) for it in [out_dir, package_dir, build_dir] + (SYSTEM_LIB_DIRS if system else []))
    def search_dir(v):
        kind, _, rest = v.partition("=")
        return rest if kind in ("native", "crate", "dependency", "framework", "all") and rest else v
    def link_arg_refused(arg, strict):
        for prefix in ("-Wl,-rpath,", "-Wl,-rpath=", "-Wl,-R"):
            if arg.startswith(prefix):
                d = arg[len(prefix):]
                relative = d.startswith(("$ORIGIN", "@loader_path", "@executable_path"))
                return not relative and (strict or not may_search(d))
        known = (arg in LINK_ARGS and (not strict or arg != "-pthread")) \
            or any(arg.startswith(p) and ".." not in arg[len(p):].split("/") for p in LINK_ARG_PREFIXES) \
            or re.fullmatch(r"-l[A-Za-z0-9_+.-]+", arg) is not None
        return not known
    out = []
    for line in stdout.splitlines():
        line = line.strip()
        if line.startswith("cargo::"):
            data = line[7:]
        elif line.startswith("cargo:") and any(line[6:].startswith(r + "=") for r in RESERVED):
            data = line[6:]
        else:
            continue
        key, _, value = data.partition("=")
        value = value.rstrip()
        if key in ("rustc-link-search", "rustc-flags"):
            dirs = [search_dir(value)] if key == "rustc-link-search" else \
                [t[2:] or n for t, n in zip(value.split(), value.split()[1:] + [""]) if t.startswith("-L")]
            if any(not may_search(d) for d in dirs):
                out.append(("link-search", line))
            elif any(not may_search(d, system=False) for d in dirs):
                out.append(("strict-link-search (system dirs not allowed)", line))
        elif key.startswith("rustc-link-arg") or key == "rustc-cdylib-link-arg":
            arg = value.partition("=")[2] if key == "rustc-link-arg-bin" else value
            if link_arg_refused(arg, strict=False):
                out.append(("link-arg", line))
            elif link_arg_refused(arg, strict=True):
                out.append(("strict-link-arg (no -pthread, no absolute rpath)", line))
        elif key == "rustc-env":
            name = value.partition("=")[0]
            if name in CARGO_GIVES or name.startswith(tuple(CARGO_GIVES_PREFIXES)):
                out.append(("rustc-env", line))
            elif name.startswith(("CARGO", "RUSTC")):
                out.append(("strict-rustc-env (any CARGO_*/RUSTC*)", line))
    return out


def directives(work, out_dir):
    """Every rule of cargo/directives.rs, and its first form, over each run below
    <work>/target: <out>/directives.md."""
    lines = collections.defaultdict(list)
    runs = 0
    for project in sorted(os.listdir(os.path.join(work, "target"))):
        for run_dir in run_dirs(os.path.join(work, "target", project)):
            try:
                ran = json.load(open(os.path.join(run_dir, RAN)))
                stdout = open(os.path.join(run_dir, "run", "stdout"), "rb").read().decode("utf-8", "replace")
            except (OSError, ValueError):
                continue
            runs += 1
            env = dict(ran["env"])
            script = f"`{env.get('CARGO_PKG_NAME')} {env.get('CARGO_PKG_VERSION')}` ({project})"
            for rule, line in directive_refusals(stdout, ran["cwd"], env.get("OUT_DIR", "")):
                lines[rule].append((script, re.sub(r"/nix/store/[0-9a-z]{32}-", "/nix/store/*-", line)))
    text = ["# What directive validation refuses over the bench corpus", "",
            f"Made by `harness/bscache.py directives` over {runs} build-script runs (`run/stdout` of each).",
            "Rules as `cargo/directives.rs` has them; `strict-*` is the rule as first written (Andrew,",
            "12:10Z), before these counts set its edges.", "",
            "| rule | lines | scripts |", "|---|---|---|"]
    for rule in ("link-search", "link-arg", "rustc-env"):
        lines.setdefault(rule, [])
    for rule in sorted(lines):
        text.append(f"| {rule} | {len(lines[rule])} | {len({s for s, _ in lines[rule]})} |")
    for rule in sorted(lines):
        if lines[rule]:
            text += ["", f"## {rule}", ""] + [f"- {s}: `{l}`" for s, l in sorted(set(lines[rule]))]
    open(os.path.join(out_dir, "directives.md"), "w").write("\n".join(text) + "\n")
    print("\n".join(text))


KEPT_LINE = "blaze: a run the store kept, of its closure"


def said_by_cargo(stderr, package):
    """What cargo printed of the failed run of `package`'s build script (`name vX`): its stderr,
    which the run's directory does not keep when it failed."""
    start = stderr.find(f"failed to run custom build command for `{package}")
    if start < 0:
        return ""
    block = stderr[start:].split("\n--- stderr\n", 1)
    rest = block[1] if len(block) == 2 else block[0]
    ends = [at for at in (rest.find("\nwarning: build failed"), rest.find("\nerror: ")) if at >= 0]
    return rest[: min(ends)] if ends else rest


def in_closure(name, repo, rev, work, blaze, jobs, store):
    """Runs `name`'s compile-time step with every build script in its closure alone
    (BLAZE_SCRIPT_VIEW=on), keeping going past a failed script, with `store` as the store: what
    each script did, from its run directory."""
    source = os.path.join(work, "src", name)
    fetch_git(repo, rev, source)
    target = os.path.join(work, "target", name)
    shutil.rmtree(target, ignore_errors=True)
    env = dict(os.environ)
    for key in list(env):
        if key.startswith(("CARGO_", "RUSTC_")) and key != "CARGO_HOME":
            del env[key]
    env.update(
        RUSTC_WRAPPER=os.path.join(os.path.dirname(blaze), "blaze-rustc"),
        BLAZE_ONLY_ASKS="1",
        BLAZE_PROC_MACRO_SERVER="0",
        BLAZE_SCRIPT_VIEW="on",
        BLAZE_STATS="1",
        BLAZE_CACHE=store,
        CARGO_TARGET_DIR=target,
        CARGO_BUILD_JOBS=str(jobs),
        CARGO_UNSTABLE_CHECKSUM_FRESHNESS="false",
        CARGO_HOME=env.get("CARGO_HOME") or os.path.join(work, "cargo-home"),
        RUST_BACKTRACE="0",
    )
    command = ["cargo", "check", "-Zunstable-options", "--compile-time-deps", "--workspace", "--all-targets", "--keep-going"]
    start = time.monotonic()
    proc = subprocess.run(command, cwd=source, env=env, capture_output=True, text=True, errors="replace")
    seconds = time.monotonic() - start
    # Cargo names a package of the workspace with its path after it.
    named = re.findall(r"failed to run custom build command for `([^`]+)`", proc.stderr)
    failed = sorted({re.sub(r" \(.*\)$", "", it) for it in named})
    scripts = []
    for dir in run_dirs(target):
        var = dict(json.load(open(os.path.join(dir, RAN)))["env"])
        # Cargo keeps a run's output beside its OUT_DIR, or in `run/` there (build-dir layout v2).
        run = os.path.join(dir, "run") if os.path.isdir(os.path.join(dir, "run")) else dir
        stdout = os.path.join(run, "stdout" if run != dir else "output")
        said = open(os.path.join(run, "stderr"), errors="replace").read() if os.path.exists(os.path.join(run, "stderr")) else ""
        package = f"{var.get('CARGO_PKG_NAME')} v{var.get('CARGO_PKG_VERSION')}"
        ok = os.path.exists(stdout) and package not in failed
        notes = [line for line in said.splitlines() if line.startswith(("note: the build script", "error:"))]
        if not ok:
            said = said or said_by_cargo(proc.stderr, package)
            notes = [line for line in said.splitlines() if line.startswith(("note: the build script", "error:"))]
        scripts.append(dict(package=var.get("CARGO_PKG_NAME"), version=var.get("CARGO_PKG_VERSION"), ok=ok,
                            kept=KEPT_LINE in said, notes=notes[-3:], tail=[] if ok else said.splitlines()[-12:]))
    # A script cargo said failed whose run left no directory of ours failed before it ran.
    ran = {f"{s['package']} v{s['version']}" for s in scripts}
    for package in failed:
        if package not in ran:
            name_, _, version = package.rpartition(" v")
            said = said_by_cargo(proc.stderr, package)
            notes = [line for line in said.splitlines() if line.startswith(("note: the build script", "error:"))]
            scripts.append(dict(package=name_, version=version, ok=False, kept=False, notes=notes[-3:],
                                tail=said.splitlines()[-12:]))
    return dict(project=name, rev=rev, status=proc.returncode, seconds=round(seconds, 2), failed=failed,
                stderr_tail=proc.stderr[-6000:] if proc.returncode else "",
                scripts=sorted(scripts, key=lambda it: (it["package"] or "", it["version"] or "")))


def closure_report(out_dir):
    """`<out>/closure.md` of the `closure` runs in `<out>`: per project the scripts that ran in
    their closure, came from the store, failed; each failed package with what it said."""
    rows, failures = [], {}
    totals = collections.Counter()
    for file in sorted(os.listdir(out_dir)):
        if not file.endswith(".json"):
            continue
        project = json.load(open(os.path.join(out_dir, file)))
        count = collections.Counter(scripts=len(project["scripts"]))
        for script in project["scripts"]:
            count["ok"] += script["ok"]
            count["kept"] += script["kept"]
            count["failed"] += not script["ok"]
            if not script["ok"]:
                entry = failures.setdefault(f"{script['package']} {script['version']}", dict(projects=set(), said=script["notes"] + script["tail"]))
                entry["projects"].add(project["project"])
        totals.update(count)
        rows.append((project["project"], project["status"], project["seconds"], count))
    lines = ["| project | cargo | seconds | scripts | ok | from the store | failed |", "|---|---|---|---|---|---|---|"]
    for name, status, seconds, c in rows + [("total", "", "", totals)]:
        lines.append(f"| {name} | {status} | {seconds} | {c['scripts']} | {c['ok']} | {c['kept']} | {c['failed']} |")
    lines += ["", "## Scripts that failed in their closure", ""]
    for package, entry in sorted(failures.items()):
        lines += [f"### `{package}` ({', '.join(sorted(entry['projects']))})", "", "```", *entry["said"][-10:], "```", ""]
    open(os.path.join(out_dir, "closure.md"), "w").write("\n".join(lines) + "\n")
    print("\n".join(lines[: len(rows) + 3]))


def main():
    argv = sys.argv[1:]
    if not argv or argv[0] not in ("record", "report", "directives", "closure", "closure-report"):
        sys.exit(__doc__)
    mode = argv.pop(0)
    out_dir = option(argv, "--out", os.path.join(HERE, "..", "bench", "results", "bscache"))
    os.makedirs(out_dir, exist_ok=True)
    if mode == "report":
        return report(out_dir)
    if mode == "closure-report":
        return closure_report(out_dir)
    if mode == "closure":
        blaze = os.path.abspath(option(argv, "--blaze") or sys.exit("--blaze BIN"))
        work = option(argv, "--work", os.path.join(os.environ.get("TMPDIR", "/tmp"), "blaze-bscache"))
        store = option(argv, "--store") or sys.exit("--store DIR")
        jobs = int(option(argv, "--jobs", "8"))
        corpus = projects()
        for name in argv or list(corpus):
            repo, rev = corpus[name]
            result = in_closure(name, repo, rev, work, blaze, jobs, store)
            json.dump(result, open(os.path.join(out_dir, f"{name}.json"), "w"), indent=1)
            ok = sum(s["ok"] for s in result["scripts"])
            kept = sum(s["kept"] for s in result["scripts"])
            print(f"{name}: cargo {result['status']} in {result['seconds']} s, {len(result['scripts'])} scripts, {ok} ok, {kept} from the store, failed {result['failed']}", flush=True)
        return
    if mode == "directives":
        return directives(option(argv, "--work", os.path.join(os.environ.get("TMPDIR", "/tmp"), "blaze-bscache")), out_dir)
    blaze = os.path.abspath(option(argv, "--blaze") or sys.exit("--blaze BIN"))
    work = option(argv, "--work", os.path.join(os.environ.get("TMPDIR", "/tmp"), "blaze-bscache"))
    jobs = int(option(argv, "--jobs", "8"))
    corpus = projects()
    names = argv or list(corpus)
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True).stdout.strip()
    allowed = dict(sysroot=sysroot, blaze_dir=os.path.dirname(blaze), closure=closure(["cc", "c++", "ar", "pkg-config"]))
    for name in names:
        repo, rev = corpus[name]
        result = record(name, repo, rev, work, blaze, jobs)
        result["allowed"] = allowed
        json.dump(result, open(os.path.join(out_dir, f"{name}.json"), "w"), indent=1)
        print(f"{name}: cargo {result['status']}, {len(result['scripts'])} scripts, failed {result['failed']}", flush=True)


if __name__ == "__main__":
    main()
