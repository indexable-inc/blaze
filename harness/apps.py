#!/usr/bin/env python3
"""apps.py: whole applications built by ra-jit, dependencies included, against cargo's build.

usage: apps.py <name> [--stage build,workload,perf,tests,edits] [--json out.json]
       apps.py --table <dir of json>

<name> is an entry of apps/apps.json. The checkout (APPS_DIR/<name>, cloned at `rev`) gets
the source of every dependency in a directory beside it (`cargo vendor`, and the source
replacement it prints in `.cargo/config.toml`), at the locked versions. An entry with `prepare`
has that module of the harness make what cargo needs before it (for bun, bunprep.py: its own
build of its C and C++, and the member rustc links). The stages:

build     cargo builds the program for the link line; ra-jit compiles every crate linked into
          it and links its objects ahead of rustc's: the program the other stages use.
workload  each command line of `runs`, cargo's program against ra-jit's: status, stdout, stderr.
perf      `bench` timed with cargo's debug and release programs and ra-jit's; sizes.
tests     the test binaries of `tests`: those of a library are linked again with ra-jit's
          objects, those that start the program are run with ra-jit's program in its place.
edits     the daemon holds the program; each edit of `edits` is timed to a linked program and
          to cargo's, and `check` has to print the same from both.

The backend is the environment's, as for the other harnesses: `RA_JIT_BACKEND`, `RA_JIT_OPT`,
`RA_JIT_EMIT`.
"""
import argparse, importlib, json, os, re, shlex, shutil, socket, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import difftest, linkline, memory
from difftest import Fixture, RA_JIT, bucket, host, sysroot_rlibs

APPS_DIR = os.environ.get("APPS_DIR") or os.path.join(HERE, "vendor", "apps")
SPECS = os.path.join(HERE, "apps", "apps.json")
# A dependency that denies warnings must still build.
FLAGS = "-C link-dead-code --cap-lints allow"


def own_flags(spec):
    """The flags the program's own build gives rustc (bun's `-Cforce-frame-pointers=yes`),
    which the harness's RUSTFLAGS would otherwise take the place of. Each build has them, and
    ra-jit reads them from RUSTFLAGS as cargo does."""
    return spec.get("rustflags", "")
STAGES = ["build", "workload", "perf", "tests", "edits"]


def sh(argv, cwd, env=None, check=True, timeout=None):
    """Runs a command, returning (seconds, result)."""
    began = time.monotonic()
    ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
    if check and ran.returncode != 0:
        raise SystemExit(f"`{shlex.join(argv)}` failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
    return time.monotonic() - began, ran


def checkout(name, spec):
    """The application's source at the pinned commit, with its dependencies beside it."""
    ws = os.path.join(APPS_DIR, name)
    if not os.path.isdir(ws):
        os.makedirs(ws)
        if "repo" in spec:
            for argv in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", spec["repo"], spec["rev"]],
                         ["git", "checkout", "-q", "FETCH_HEAD"],
                         ["git", "submodule", "update", "-q", "--init", "--recursive", "--depth", "1"]):
                sh(argv, ws)
        else:
            shutil.copytree(os.path.join(HERE, "apps", spec["source"]), ws, dirs_exist_ok=True)
    return os.path.join(ws, spec.get("dir", ""))


def prepare(ws, spec):
    """What the application's own build makes before cargo can build it: `prepare` names a
    module of the harness whose `in_place(ws)` makes it, once, and returns the environment
    every build of the workspace needs (bunprep.py)."""
    if "prepare" in spec:
        os.environ.update(importlib.import_module(spec["prepare"]).in_place(ws))


def localize(ws):
    """Puts the source of every dependency in a directory beside the workspace and has cargo
    take it from there, so the versions built are the locked ones and ra-jit reads the files
    cargo compiles."""
    config = os.path.join(ws, ".cargo", "config.toml")
    marker = "# apps.py: dependencies in a directory\n"
    if os.path.exists(config) and marker in open(config).read():
        return
    vendor = os.path.abspath(ws).rstrip("/") + "-vendor"
    replaced = sh(["cargo", "vendor", "--locked", "--versioned-dirs", vendor], ws)[1].stdout
    os.makedirs(os.path.dirname(config), exist_ok=True)
    with open(config, "a") as out:
        out.write("\n" + marker + replaced)


def package_directory(ws, package):
    """Where the package's manifest is. ra-jit is asked for the package and not the workspace:
    a build of one member has the features that member asks for, not every member's."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ws))
    for found in metadata["packages"]:
        if found["name"] == package:
            return os.path.dirname(found["manifest_path"])
    raise SystemExit(f"no package {package} in {ws}")


class App(Fixture):
    """A program and every crate linked into it."""

    def __init__(self, ws, spec):
        self.spec = spec
        super().__init__(ws, [], ["-p", spec["package"], *spec.get("cargo_args", [])])
        self.reports = {}
        self.scrubbed = spec.get("scrub", [])
        self.argv0 = spec.get("bin", spec["package"])
        self.run_env = {**os.environ, **spec.get("env", {})}
        self.stdin = None
        self.project = package_directory(ws, spec["package"])

    def rustc_build(self):
        """Cargo's build, the reference ra-jit's objects are linked ahead of."""
        began = time.monotonic()
        super().rustc_build()
        self.cargo_secs = time.monotonic() - began
        linked = {os.path.basename(r)[3:].rsplit("-", 1)[0] for r in self.rlibs
                  if r.endswith(".rlib") and "/rustlib/" not in r}
        program = os.path.basename(self.reference).rsplit("-", 1)[0].replace("-", "_")
        self.crates = [",".join([program, *sorted(linked - {program})])]

    def ra_jit_build(self, deny=()):
        """As the fixture's, keeping what the process took: seconds and peak resident memory."""
        crate = self.crates[0]
        obj, report, log = (os.path.join(self.work, f"all.{ext}") for ext in ("o", "tsv", "stderr"))
        symbols = [x for rlib in self.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        argv = [RA_JIT, self.project, *(x for c in crate.split(",") for x in ("--crate", c)), "--out", obj,
                "--report", report, *self.ra_jit_args, *symbols]
        with open(os.path.join(self.work, "all.argv"), "w") as out:
            out.write(shlex.join(argv))
        began = time.monotonic()
        with open(log, "w") as err:
            child = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=err,
                                     env={**os.environ, "RUST_BACKTRACE": "1"})
            _, status, usage = os.wait4(child.pid, 0)
        self.ra_jit_secs, self.ra_jit_mb = time.monotonic() - began, memory.maxrss_mb(usage)
        if status != 0:
            raise SystemExit(f"ra-jit crashed:\n{open(log).read()[-3000:]}")
        self.reports = {crate: [l.rstrip("\n").split("\t") for l in open(report)]}
        self.objects = [obj]
        began = time.monotonic()
        self.link(self.objects)
        self.link_secs = time.monotonic() - began

    def behaviour(self, binary):
        stdin = open(os.path.join(self.ws, self.stdin), "rb") if self.stdin else subprocess.DEVNULL
        try:
            run = subprocess.run([self.argv0, *self.run_args], executable=binary, capture_output=True,
                                 stdin=stdin, timeout=600, cwd=self.ws, env=self.run_env)
        except subprocess.TimeoutExpired:
            return "timeout", b"", b""
        # A program that prints where it is prints a different place for each build.
        where, directory = os.fsencode(binary), os.fsencode(os.path.dirname(binary))
        return run.returncode, *(self.scrub(out.replace(where, b"<program>").replace(directory, b"<directory>"))
                                 for out in (run.stdout, run.stderr))

    def scrub(self, output):
        """What a program measures (elapsed times) differs from run to run."""
        for pattern in self.scrubbed:
            output = re.sub(pattern.encode(), b"<scrubbed>", output)
        return output

    def summary(self):
        """Functions compiled and left, the ones left by reason."""
        rows = self.reports[self.crates[0]]
        reasons, examples = {}, {}
        for row in rows:
            if row[0] == "skipped" and bucket(row[2]) != "no rustc symbol for function":
                why = cause(row[2])
                reasons[why] = reasons.get(why, 0) + 1
                examples.setdefault(why, f"{row[1]}: {row[2][:300]}")
        return {"compiled": sum(r[0] == "compiled" for r in rows), "left": sum(reasons.values()),
                "reasons": reasons, "examples": examples, "instances": int(rows[0][2]),
                "codegen_s": round(int(rows[0][4]) / 1e6, 1), "crates": len(self.crates[0].split(","))}


def cause(reason):
    """Why a function was left, without what differs from one function to the next: the
    chain of callees it was found through, and the names in backquotes."""
    last = re.split(r"callee `[^`]*`: ", reason)[-1]
    last = re.sub(r"^(unsupported|frontend|backend): ", "", last)
    return re.sub(r"`[^`]*`", "`..`", last)[:120]


def build(ws, spec, result):
    """One build of the program by ra-jit; returns the fixture, or None with the failure noted."""
    key = "ahead"
    fx = App(ws, spec)
    try:
        fx.rustc_build()
        fx.ra_jit_build()
    except SystemExit as failed:
        result[key] = {"error": str(failed)[-1500:], "work": fx.work, **(fx.summary() if fx.reports else {})}
        missing = sorted(set(re.findall(r"undefined symbol: (.*)", str(failed))))
        print(f"{key}: FAILED {str(failed)[-3000:]}")
        for symbol in missing[:20]:
            print(f"  undefined {symbol[:200]}")
        reasons(result[key])
        return None
    result[key] = {**fx.summary(), "cargo_s": round(fx.cargo_secs, 1), "ra_jit_s": round(fx.ra_jit_secs, 1),
                   "link_s": round(fx.link_secs, 1), "peak_mb": fx.ra_jit_mb,
                   "size": os.path.getsize(fx.binary), "work": fx.work}
    row = result[key]
    print(f"{key}: {row['compiled']} functions from ra-jit over {row['crates']} crates, {row['left']} left; "
          f"ra-jit {row['ra_jit_s']} s, link {row['link_s']} s, peak {row['peak_mb']} MB")
    reasons(row)
    return fx


def reasons(row):
    """Prints why functions were left, the most frequent reason first."""
    for reason, count in sorted(row.get("reasons", {}).items(), key=lambda it: -it[1]):
        print(f"  {count:5} {reason}\n        {row['examples'][reason]}")


def reference(ws, spec, profile, timed=False):
    """Cargo's own build of the program without the harness's flags, timed when it is from
    nothing or when asked; returns (program, seconds or None)."""
    target = os.path.join(ws, "target-" + profile)
    name = spec.get("bin", spec["package"])
    program = os.path.join(target, profile, name)
    cold = not os.path.exists(program)
    env = {**os.environ, "CARGO_TARGET_DIR": target, "RUSTFLAGS": f"--cap-lints allow {own_flags(spec)}".strip()}
    argv = ["cargo", "build", "-p", spec["package"], *spec.get("cargo_args", []),
            *(["--release"] if profile == "release" else [])]
    secs, _ = sh(argv, ws, env)
    return program, round(secs, 1) if cold or timed else None


def setup(ws, spec):
    """What the command lines read is made once."""
    done = os.path.join(ws, ".apps-setup")
    if not os.path.exists(done):
        for line in spec.get("setup", []):
            sh(["bash", "-c", line], ws)
        open(done, "w").close()


class Served:
    """The program as a server on a free local port for as long as the block runs, when the
    application is one (`serve` are its arguments); `{port}` in a command line is its port."""

    def __init__(self, fx, binary):
        self.fx, self.binary, self.child, self.port = fx, binary, None, 0

    def __enter__(self):
        serve = self.fx.spec.get("serve")
        if serve is None:
            return self
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        self.child = subprocess.Popen([self.fx.argv0, *self.args(serve)], executable=self.binary, cwd=self.fx.ws,
                                      env=self.fx.run_env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and self.child.poll() is None:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                return self
            except OSError:
                time.sleep(0.05)
        self.__exit__()
        raise SystemExit(f"the server did not start: {self.child.stderr.read().decode(errors='replace')[-2000:]}")

    def __exit__(self, *_):
        if self.child:
            self.child.kill()
            self.child.wait()

    def args(self, argv):
        return [arg.replace("{port}", str(self.port)) for arg in argv]

    def behaviour(self, run):
        """A command line of the program, or of a tool (`{"tool": [...]}`) that talks to it."""
        if isinstance(run, dict) and "tool" in run:
            ran = subprocess.run(self.args(run["tool"]), capture_output=True, stdin=subprocess.DEVNULL,
                                 timeout=600, cwd=self.fx.ws)
            return ran.returncode, self.fx.scrub(ran.stdout), self.fx.scrub(ran.stderr)
        if isinstance(run, dict):
            # The program reads the file `stdin` names.
            self.fx.run_args, self.fx.stdin = self.args(run["args"]), run["stdin"]
        else:
            self.fx.run_args, self.fx.stdin = self.args(run), None
        return self.fx.behaviour(self.binary)

    def seconds(self, argv, rounds=3):
        """The fastest of a few runs of a command line of the program."""
        times = []
        for _ in range(rounds):
            began = time.monotonic()
            subprocess.run([self.fx.argv0, *self.args(argv)], executable=self.binary, cwd=self.fx.ws,
                           capture_output=True, stdin=subprocess.DEVNULL, env=self.fx.run_env)
            times.append(time.monotonic() - began)
        return round(min(times), 3)

    def load(self, argv):
        """Requests a second that wrk got out of the server, and how many were not answered
        with success."""
        out = subprocess.run(self.args(argv), capture_output=True, text=True, cwd=self.fx.ws).stdout
        rate = re.search(r"Requests/sec:\s*([0-9.]+)", out)
        failed = re.search(r"Non-2xx or 3xx responses: (\d+)", out)
        errors = re.search(r"Socket errors: (.*)", out)
        return {"rps": float(rate.group(1)) if rate else 0.0, "failed": int(failed.group(1)) if failed else 0,
                "errors": errors.group(1) if errors else ""}


def name_of(run):
    if isinstance(run, dict):
        return shlex.join(run["tool"]) if "tool" in run else f"{shlex.join(run['args'])} < {run['stdin']}"
    return shlex.join(run)


def workload(fx, spec, debug, result):
    """Every command line with cargo's program, then with ra-jit's, in the same order: a
    server keeps what the earlier ones did to it."""
    seen = []
    for binary in (debug, fx.binary):
        with Served(fx, binary) as served:
            seen.append([served.behaviour(run) for run in spec["runs"]])
    differing = []
    for run, want, got in zip(spec["runs"], *seen):
        if want != got:
            differing.append(name_of(run))
            tail = got[2].decode(errors="replace").strip().splitlines()[-2:]
            print(f"  DIFFERS `{name_of(run)}`: rustc status {want[0]}, ra-jit status {got[0]}: "
                  f"{difftest.symptom(want, got)} {' | '.join(tail)[:300]}")
    result["workload"] = {"runs": len(spec["runs"]), "differing": differing}
    print(f"workload: {len(spec['runs']) - len(differing)} of {len(spec['runs'])} command lines the same")


def perf(fx, spec, debug, release, result):
    """`bench` timed, or `load` put on the server, with each of the three programs."""
    row = {"debug_size": os.path.getsize(debug), "release_size": os.path.getsize(release)}
    for key, binary in (("ra_jit", fx.binary), ("debug", debug), ("release", release)):
        with Served(fx, binary) as served:
            if "load" in spec:
                row.update({f"{key}_{k}": v for k, v in served.load(spec["load"]).items()})
            else:
                row[f"{key}_s"] = served.seconds(spec["bench"])
    result["perf"] = row
    if "load" in spec:
        row["bench"] = shlex.join(spec["load"])
        print(f"load `{row['bench']}`: ra-jit {row['ra_jit_rps']} requests/s ({row['ra_jit_failed']} failed), "
              f"rustc debug {row['debug_rps']} ({row['debug_failed']}), release {row['release_rps']} ({row['release_failed']})")
    else:
        row["bench"] = shlex.join(spec["bench"])
        print(f"perf `{row['bench']}`: ra-jit {row['ra_jit_s']} s, rustc debug {row['debug_s']} s, release {row['release_s']} s")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("name", nargs="?")
    ap.add_argument("--stage", default="build,workload,perf")
    ap.add_argument("--json")
    ap.add_argument("--table")
    a = ap.parse_args()
    if a.table:
        import appstable
        return appstable.main(a.table)
    spec = json.load(open(SPECS))[a.name]
    stages = a.stage.split(",")
    out = os.path.abspath(a.json) if a.json else None
    # What the build scripts of the application read.
    os.environ.update(spec.get("build_env", {}))
    ws = checkout(a.name, spec)
    prepare(ws, spec)
    localize(ws)
    os.environ["RUSTFLAGS"] = f"{FLAGS} {own_flags(spec)}".strip()
    backend = os.environ.get("RA_JIT_BACKEND", "cranelift")
    result = {"app": a.name, "backend": backend, "rev": spec.get("rev")}
    debug, result["debug_cold_s"] = reference(ws, spec, "debug")
    setup(ws, spec)
    fx = build(ws, spec, result)
    result["program"] = "ahead" if fx else None
    ok = fx is not None
    if fx and "workload" in stages:
        workload(fx, spec, debug, result)
        ok = ok and not result["workload"]["differing"]
    if fx and "perf" in stages and ("bench" in spec or "load" in spec):
        release, result["release_cold_s"] = reference(ws, spec, "release")
        perf(fx, spec, debug, release, result)
    if fx and "tests" in stages:
        import appstests
        ok = appstests.run(fx, ws, spec, result) and ok
    if fx and "edits" in stages:
        import appsedits
        ok = appsedits.run(fx, ws, spec, result) and ok
    result["ok"] = ok
    if out:
        json.dump(result, open(out, "w"), indent=1)
    print("OK" if ok else "FAILED")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
