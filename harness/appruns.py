"""appruns.py: the applications of apps/apps.json checked out, prepared and run, for a harness
that builds them itself and compares what the programs do.

An entry's checkout (APPS_DIR/<name>, cloned at `rev`) gets the source of every dependency in a
directory beside it (`cargo vendor`, and the source replacement it prints in
`.cargo/config.toml`), at the locked versions. An entry with `prepare` has that module of the
harness make what cargo needs before it (for bun, bunprep.py). `Program` runs a built program
of an entry as its `runs` say, with what differs from run to run written alike; `Served` runs it
as a server for as long as a block runs, when the entry is one.
"""
import importlib, json, os, re, shlex, shutil, socket, subprocess, time

HERE = os.path.dirname(os.path.abspath(__file__))

APPS_DIR = os.environ.get("APPS_DIR") or os.path.join(HERE, "vendor", "apps")
SPECS = os.path.join(HERE, "apps", "apps.json")


def own_flags(spec):
    """The flags the program's own build gives rustc (bun's `-Cforce-frame-pointers=yes`),
    which the harness's RUSTFLAGS would otherwise take the place of. Each build has them, and
    blaze reads them from RUSTFLAGS as cargo does."""
    return spec.get("rustflags", "")


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
    take it from there, so the versions built are the locked ones and blaze reads the files
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
    """Where the package's manifest is."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ws))
    for found in metadata["packages"]:
        if found["name"] == package:
            return os.path.dirname(found["manifest_path"])
    raise SystemExit(f"no package {package} in {ws}")


def setup(ws, spec):
    """What the command lines read is made once."""
    done = os.path.join(ws, ".apps-setup")
    if not os.path.exists(done):
        for line in spec.get("setup", []):
            sh(["bash", "-c", line], ws)
        open(done, "w").close()


class Program:
    """An entry's program, run as its `runs` say; `touch` is the root file of its binary."""

    def __init__(self, ws, spec):
        self.ws, self.spec = os.path.abspath(ws), spec
        self.run_args = []
        self.stdin = None
        self.scrubbed = spec.get("scrub", [])
        self.argv0 = spec.get("bin", spec["package"])
        self.run_env = {**os.environ, **spec.get("env", {})}
        self.project = package_directory(ws, spec["package"])
        meta = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=self.ws))
        self.touch = next(t["src_path"] for p in meta["packages"] if p["name"] == spec["package"]
                          for t in p["targets"] if "bin" in t["kind"])

    def behaviour(self, binary):
        """(exit status, stdout, stderr) of a run of `binary`; ("timeout", b"", b"") past 600 s."""
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
            # The program reads the file `stdin` names, if any.
            self.fx.run_args, self.fx.stdin = self.args(run["args"]), run.get("stdin")
        else:
            self.fx.run_args, self.fx.stdin = self.args(run), None
        return self.fx.behaviour(self.binary)


def name_of(run):
    if isinstance(run, dict):
        if "tool" in run:
            return shlex.join(run["tool"])
        return shlex.join(run["args"]) + (f" < {run['stdin']}" if run.get("stdin") else "")
    return shlex.join(run)


def symptom(expected, got):
    """How the run `got` differs from `expected`, both as `Program.behaviour` gives them: the
    exit status, else the first line of standard output that differs."""
    if got[0] != expected[0]:
        return f"exit status {got[0]}"
    want, have = expected[1].decode(errors="replace").splitlines(), got[1].decode(errors="replace").splitlines()
    for w, h in zip(want, have):
        if w != h:
            return f"expected `{w}`, got `{h}`"
    return "output length differs"
