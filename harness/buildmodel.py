#!/usr/bin/env python3
"""buildmodel.py: which parts of cargo's build model blaze honours.

usage: buildmodel.py [case ...] [--backend cranelift|llvm] [--table FILE] [--keep]

Each directory under `harness/buildmodel/` is a small cargo project that depends on one feature
of the build model, with a `case.json` that says how cargo is asked to build it and which
crates are blaze's. The project is built twice: by cargo and rustc alone, which is the
reference, and by cargo with blaze's code for the named crates linked in front of rustc's.
Both programs run and what they print and return is compared. A case may then change one of
its inputs a step at a time under one daemon, and each step is compared with cargo the same way.

  A row is one of
  supported  the same output, every function of the named crates compiled by blaze
  refused    blaze said why it does not build this, and built nothing that runs differently
  WRONG      different output, a crash, or a refusal without a reason
  open       WRONG where the case's `open` says it is known to be, and why
  skipped    the case is of another host: a Linux cross target has its linker and runner there
and the run fails on any WRONG row and on any row that is not what its `case.json` expects.

`case.json`:
  row       what the case is of, as the table names it
  package   the package cargo is asked for (`-p`)
  select    what of the package is built when not its one program: ["--example", "demo"]
  verb      "build" (the default), "test" or "bench": what cargo is asked to do with it
  artifact  the name of the target that is run, when it is not the selected one or the package
  archive   the build is an archive and has no link line: blaze's objects are given to `run`
  unbuilt   why nothing of blaze can be asked to build this at all: the row is that refusal
  doctests  the case is `cargo test --doc` of the package: once with rustdoc, once with blaze
            as cargo's RUSTDOC, and what the test harness prints and returns is compared
  link      more arguments to cargo for the build blaze links against, which the reference
            is built without
  says      what blaze has to say on stderr as it builds
  crates    the crates blaze compiles
  cargo     more arguments to cargo: ["--release"], ["--features", "x"]
  blaze     more arguments to blaze
  target    the triple to build for when it is not the host
  on        the architecture of the only hosts the case is of: it is not run on another
  env       the environment of both builds
  run       a script of the case that runs what was built in place of running it directly;
            it is given ARTIFACT, and BLAZE_OBJECTS with blaze's objects for the second build
  args      what the program is run with
  expect    "supported" (the default) or "refused"
  open      why the case is known to come out WRONG, or {"linux" | "darwin" | "windows": why} where
            only on those hosts. It is shown as open, and fails the run once it does not.
  handed    blaze is given the link line of the build and links by itself
  fails     cargo itself turns the project down, and so must blaze
  steps     [{"what": .., "edit": [[file, old, new]], "write": {file: text}, "env": {..},
              "run": [shell commands in the project], "clients": how many ask at once,
              "paused": the clients ask while the daemon is held in the middle of the reload,
              "stale": the client asks with what cargo linked before the step, as one
              that leaves the build to the daemon does,
              "expect": "same" | "refused",
              "links": "spliced": the daemon splices the edit into what it linked before}]
            After each step cargo builds again and the client names what it linked.
"""
from concurrent.futures import ThreadPoolExecutor
import errno, argparse, glob, json, os, re, shlex, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.pop("BLAZE_TARGET", None)
import linkline
from difftest import BLAZE, host, linker_line

# A case names its own flags: difftest's default would replace the ones of its configuration.
os.environ.pop("RUSTFLAGS", None)

CASES = os.path.join(HERE, "buildmodel")
# blaze needs the symbols of what only one codegen unit uses: see difftest.py.
KEEP = ["-C", "link-dead-code"]
CROSS = {
    "aarch64-unknown-linux-gnu": {"linker": "aarch64-linux-gnu-gcc", "runner": ["qemu-aarch64", "-L", "/usr/aarch64-linux-gnu"]},
    "x86_64-unknown-linux-musl": {"linker": "musl-gcc", "runner": []},
}


class Refused(Exception):
    """blaze turned the build down and said why."""


class Wrong(Exception):
    """blaze's build is not cargo's, or it failed without a reason."""


def sysroot_rlibs(triple, env):
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True, env=env).strip()
    return [p for n in ("core", "alloc", "std") for p in glob.glob(f"{sysroot}/lib/rustlib/{triple}/lib/lib{n}-*.rlib")]


def reason_of(stderr):
    """What blaze gave as the reason it stopped: its last line, unless it crashed."""
    lines = [l for l in stderr.strip().splitlines() if l.strip()]
    if "link failed" in stderr or "LNK" in stderr:
        print("DEBUG-LINK " + repr(stderr[-20000:]), file=sys.stderr, flush=True)
    if not lines:
        raise Wrong("blaze failed and said nothing")
    if any("panicked at" in l for l in lines) or "stack backtrace" in stderr:
        raise Wrong("blaze crashed: " + next(l for l in lines if "panicked at" in l or "backtrace" in l)[:200])
    return re.sub(r"^Error: ", "", lines[-1])[:300]


class Case:
    def __init__(self, name, backend, keep):
        self.name, self.backend, self.keep = name, backend, keep
        self.source = os.path.join(CASES, name)
        self.spec = json.load(open(os.path.join(self.source, "case.json")))
        self.target = self.spec.get("target")
        # A target that is the host's own is linked and run as the host's programs are.
        cross = CROSS.get(self.target, {}) if self.target != host() else {}
        self.runner = cross.get("runner", [])
        self.env = {**os.environ, **self.spec.get("env", {})}
        if "linker" in cross:
            self.env["CARGO_TARGET_" + self.target.upper().replace("-", "_") + "_LINKER"] = cross["linker"]
        if backend == "llvm":
            self.env.update(BLAZE_BACKEND="llvm", BLAZE_OPT="2", BLAZE_EMIT="fast")
        else:
            self.env["BLAZE_BACKEND"] = "cranelift"
        self.select = ["-p", self.spec["package"], *self.spec.get("select", [])]
        self.cargo_args = [*self.spec.get("cargo", []), *(["--target", self.target] if self.target else [])]
        self.blaze_args = ["--package", self.spec["package"], *self.spec.get("blaze", []), *(["--target", self.target] if self.target else [])]
        self.crates = self.spec["crates"]
        self.verb = self.spec.get("verb", "build")

    def prepare(self):
        # By its long name: the temporary directory of a Windows runner is named by a short one.
        self.ws = os.path.realpath(tempfile.mkdtemp(prefix=f"blaze-bm-{self.name}-"))
        shutil.copytree(self.source, self.ws, dirs_exist_ok=True, ignore=shutil.ignore_patterns("target*", "case.json"))
        self.work = os.path.join(self.ws, "target-blaze-out")
        os.makedirs(self.work)
        if os.path.exists(os.path.join(self.ws, "setup.sh")):
            # Where the copy is as a `file://` URL has it, which on Windows is not what `sh` calls it.
            here = self.ws.replace(os.sep, "/")
            self.env["BM_HERE"] = here if here.startswith("/") else "/" + here
            setup = subprocess.run(["sh", "setup.sh"], cwd=self.ws, capture_output=True, text=True, env=self.env)
            if setup.returncode != 0:
                raise Wrong("the case's setup.sh failed: " + (setup.stderr + setup.stdout)[-400:])

    # The reference.

    def cargo(self, env, target_dir="target-ref"):
        """Builds with cargo alone: the artifact, or None when cargo turns the project down."""
        verb = {"build": ["build"], "test": ["test", "--no-run"], "bench": ["bench", "--no-run"]}[self.verb]
        run = subprocess.run(["cargo", *verb, *self.select, *self.cargo_args, "--message-format=json"], cwd=self.ws,
                             capture_output=True, text=True, env={**env, "CARGO_TARGET_DIR": os.path.join(self.ws, target_dir)})
        if run.returncode != 0:
            self.cargo_error = run.stderr[-1500:]
            return None
        made = [json.loads(l) for l in run.stdout.splitlines() if l.startswith("{")]
        made = [m for m in made if m.get("reason") == "compiler-artifact" and m["target"]["name"] == self.artifact_name()]
        last = made[-1]
        return last.get("executable") or last["filenames"][0]

    def sysroot(self, env):
        return subprocess.check_output(["rustc", "--print", "sysroot"], text=True, env=env, cwd=self.ws).strip()

    def artifact_name(self):
        return self.spec.get("artifact") or (self.select[3] if len(self.select) > 3 else self.spec["package"])

    def behaviour(self, artifact, env, objects=()):
        """What running the build prints and returns."""
        if "run" in self.spec:
            command = ["sh", self.spec["run"]]
            env = {**env, "ARTIFACT": artifact, "BLAZE_OBJECTS": " ".join(objects)}
        else:
            command = [*self.runner, artifact, *self.spec.get("args", [])]
        # A program linked to a `dylib` finds it where cargo left it, as `cargo run` has it.
        mine, other = ("target-ref", "target") if "/target-ref/" in artifact else ("target", "target-ref")
        # `deps` in cargo's first layout of the build directory, the `out` of each unit that
        # built one in its second.
        libraries = [d for t in (mine, other) for d in [
            *glob.glob(os.path.join(self.ws, t, "**", "deps"), recursive=True),
            *sorted({os.path.dirname(lib) for lib in glob.glob(
                os.path.join(self.ws, t, "**", "build", "*", "*", "out", "lib*.so"), recursive=True)})]]
        libraries += glob.glob(os.path.join(self.sysroot(env), "lib", "rustlib", "*", "lib"))
        env = {**env, "LD_LIBRARY_PATH": ":".join(libraries)}
        try:
            run = subprocess.run(command, cwd=self.ws, capture_output=True, timeout=120, env=env)
        except subprocess.TimeoutExpired:
            return ("timeout", b"", b"")
        clean = lambda out: re.sub(rb"finished in [0-9.]+s", b"finished", re.sub(rb"' \(\d+\) panicked", b"' panicked", out))
        return (run.returncode, clean(run.stdout), clean(run.stderr))

    # blaze's build.

    def link_line(self, env):
        """cargo's build of what blaze links against, and the line that links it."""
        env = dict(env)
        # A build of the whole workspace has no one rustc to hand arguments: every one prints.
        whole = "--workspace" in self.cargo_args
        ask = ["--print", "link-args", "-C", "save-temps"]
        flags = KEEP + (ask if whole else [])
        if "RUSTFLAGS" in env:
            env["RUSTFLAGS"] += " " + " ".join(flags)
            config = []
        else:
            # Joined with what the project's own configuration has, which RUSTFLAGS would replace.
            config = ["--config", "build.rustflags=" + json.dumps(flags)]
        profile = ["--profile", self.verb] if self.verb != "build" else []
        config += self.spec.get("link", [])
        command = ["cargo", "build", *config, *self.cargo_args] if whole else \
            ["cargo", "rustc", *config, *self.select, *profile, *self.cargo_args, "--", *ask]
        run = subprocess.run(command, cwd=self.ws, capture_output=True, text=True, env=env)
        if run.returncode != 0:
            raise Wrong("cargo failed with what blaze needs of it: " + run.stderr[-600:])
        printed = run.stdout + run.stderr
        if whole:
            # The program is `deps/<name>-<hash>` in cargo's first layout of the build directory
            # and `build/<name>/<hash>/out/<name>` in its second, where the build script's
            # line also has the package's directory in it. On Windows rustc prints each argument
            # quoted as Rust's Debug does, every `\` doubled.
            name = re.escape(self.artifact_name())
            program = rf"[/\\]{name}-|[/\\]+out[/\\]+{name}(\.exe)?[\"'\s]"
            printed = "\n".join(l for l in printed.splitlines() if re.search(program, l))
        try:
            # rustc's `symbols.o` asks for what the libraries need of one another, which a linker
            # that reads archives in order finds no other way: `-C save-temps` keeps the file.
            line = linker_line(printed)
        except SystemExit:
            return None
        # A DLL of MSVC exports what the directives of `symbols.o` name: its `.def` is empty.
        if linkline.msvc(line):
            exports = any(a.upper() == "/DLL" for a in line)
            return [a for a in line if exports or not a.endswith("symbols.o")]
        return [a for a in line if not a.endswith("symbols.o") or os.path.exists(a)]

    def archive(self, env):
        """cargo's archive of the package with every symbol kept, in place of a link line."""
        built = self.cargo({**env, "RUSTFLAGS": " ".join([env.get("RUSTFLAGS", ""), *KEEP]).strip()}, "target")
        if built is None:
            raise Wrong("cargo failed with what blaze needs of it: " + self.cargo_error[-600:])
        return [built]

    def link_dir(self):
        """Where cargo runs the linker: the root of the workspace."""
        return self.ws

    def symbols(self, link, env):
        rlibs = [a for a in link if a.endswith((".rlib", ".rcgu.o", ".a"))]
        return [x for rlib in rlibs + sysroot_rlibs(self.target or host(), env) for x in ("--symbols", rlib)]

    def left_to_rustc(self, report):
        rows = [l.rstrip("\n").split("\t") for l in open(report)]
        compiled = sum(r[0] == "compiled" for r in rows)
        skipped = [r for r in rows if r[0] == "skipped" and "no rustc symbol for function" not in r[2]]
        return compiled, skipped

    def batch(self, env):
        """Builds once with blaze, not as a daemon: the artifact and how many functions are blaze's."""
        link = self.link = self.archive(env) if self.spec.get("archive") else self.link_line(env)
        if link is None:
            raise Wrong("cargo printed no link line for blaze's objects to go on")
        obj, report = os.path.join(self.work, "blaze.o"), os.path.join(self.work, "report.tsv")
        out = os.path.join(self.work, "out")
        os.makedirs(out, exist_ok=True)
        binary = os.path.join(out, os.path.basename(linkline.output_of(link))) if not self.spec.get("archive") else None
        handed = []
        if self.spec.get("handed"):
            handed = ["--link-cmd", linkline.join(linkline.with_output(linkline.ahead(link, [obj], BLAZE), binary))]
        run = subprocess.run(
            [BLAZE, self.ws, *(x for c in self.crates for x in ("--crate", c)), "--out", obj, "--report", report,
             *self.blaze_args, *handed, *self.symbols(link, env)],
            cwd=self.link_dir(), capture_output=True, text=True, env=env, timeout=600)
        open(os.path.join(self.work, "blaze.stderr"), "w").write(run.stderr)
        if run.returncode < 0:
            raise Wrong(f"blaze died of signal {-run.returncode}: " + run.stderr[-300:])
        if run.returncode != 0:
            raise Refused(reason_of(run.stderr))
        if self.spec.get("says", "") not in run.stderr:
            raise Wrong(f"blaze did not say `{self.spec['says']}`")
        compiled, skipped = self.left_to_rustc(report)
        if skipped:
            raise Refused(f"{len(skipped)} functions left to rustc: {skipped[0][1]}: {skipped[0][2][:160]}")
        if not compiled:
            raise Wrong("blaze compiled nothing, so nothing was compared")
        if self.spec.get("archive"):
            return link[0], [obj], compiled
        if handed:
            return binary, [], compiled
        linked = subprocess.run(linkline.with_output(linkline.ahead(link, [obj], BLAZE), binary), cwd=self.link_dir(), capture_output=True, text=True, env=env)
        if linked.returncode != 0:
            raise Wrong("the link with blaze's objects failed: " + (linked.stderr + linked.stdout)[-400:])
        return binary, [], compiled

    # One input changed at a time under one daemon.

    def follow(self, env):
        """Runs the steps of the case against one daemon: (what, verdict, detail) of each."""
        link = self.link
        obj, patch, binary = (os.path.join(self.work, f"daemon.{n}") for n in ("o", "patch.o", "bin"))
        # The path of a socket has a limit that the name of a case can take the copy past.
        socket, log_path = os.path.join(tempfile.mkdtemp(prefix="bm-"), "s"), os.path.join(self.work, "daemon.log")
        self.daemon_log = log_path
        self.daemon_files = (obj, patch, binary)
        ask = self.ask(link, env)
        log = open(log_path, "a")
        daemon = subprocess.Popen(
            [BLAZE, self.ws, *(x for c in self.crates for x in ("--crate", c)), *self.blaze_args, "--socket", socket, *ask],
            stderr=log, stdout=log, env=env)
        try:
            while "watching" not in open(log_path).read():
                if daemon.poll() is not None:
                    return [("the daemon starts", "WRONG", open(log_path).read()[-300:])]
                time.sleep(0.05)
            self.last_asked = ["--crate", self.crates[0], *ask]
            return [self.step(step, env, daemon, socket, binary) for step in self.spec["steps"]]
        finally:
            daemon.kill()
            shutil.rmtree(os.path.dirname(socket), ignore_errors=True)

    def ask(self, link, env):
        """What a client names the program by, and how it is linked with what cargo built."""
        obj, patch, binary = self.daemon_files
        line = linkline.join(linkline.with_output(linkline.ahead(link, [patch, obj], BLAZE), binary))
        return ["--out", obj, "--patch", patch, "--link-cmd", line, "--binary", binary, *self.symbols(link, env)]

    def asked(self, env):
        """The program as cargo links it now: what a client sends after cargo has built again.
        Nothing when cargo does not build it, where the daemon is to say why by itself."""
        link = self.link_line(env)
        if link is None:
            # cargo has built exactly this before and links nothing: without what it linked
            # then, it does again.
            # In `deps` in cargo's first layout; in the `out` of its unit in the second, where
            # only a test has the hash in its name.
            name = self.artifact_name()
            for linked in [*glob.glob(os.path.join(self.ws, "target", "**", "deps", name + "-*"), recursive=True),
                           *glob.glob(os.path.join(self.ws, "target", "**", "build", "*", "*", "out", name), recursive=True),
                           *glob.glob(os.path.join(self.ws, "target", "**", "build", "*", "*", "out", name + "-*"), recursive=True)]:
                if "." not in os.path.basename(linked):
                    os.remove(linked)
            link = self.link_line(env)
        return [] if link is None else ["--crate", self.crates[0], *self.ask(link, env)]

    def step(self, step, env, daemon, socket, binary):
        for path, old, new in step.get("edit", []):
            text = open(os.path.join(self.ws, path)).read()
            assert old in text, (self.name, step["what"], old)
            open(os.path.join(self.ws, path), "w").write(text.replace(old, new, 1))
        for path, text in step.get("write", {}).items():
            os.makedirs(os.path.dirname(os.path.join(self.ws, path)), exist_ok=True)
            open(os.path.join(self.ws, path), "w").write(text)
        env.update(step.get("env", {}))
        for command in step.get("run", []):
            subprocess.run(command, shell=True, cwd=self.ws, check=True, capture_output=True, env=env)
        time.sleep(0.02)
        before = self.behaviour(binary, env) if step.get("paused") else None
        if not step.get("stale"):
            self.last_asked = self.asked(env)
        ask = [BLAZE, "sync", socket, *self.last_asked]
        logged = os.path.getsize(self.daemon_log)
        if step.get("paused"):
            return self.mid_reload(step, env, daemon, ask, binary, before)
        # Clients that ask at once race the reload: each is answered from one model.
        racing = [subprocess.Popen(ask, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
                  for _ in range(step.get("clients", 1) - 1)]
        reply = subprocess.run(ask, capture_output=True, text=True, timeout=300, env=env)
        reply = (reply.stdout + reply.stderr).strip()
        for other in racing:
            said = other.communicate(timeout=300)[0].strip()
            if not said.startswith("ok"):
                reply = said
        artifact = self.cargo(env)
        expect = step.get("expect", "same")
        if daemon.poll() is not None:
            return (step["what"], "WRONG", "the daemon died")
        if artifact is None:
            verdict = ("refused", reply) if not reply.startswith("ok") else ("WRONG", "cargo turns it down, blaze answered ok")
        elif not reply.startswith("ok"):
            verdict = ("refused", reply)
        else:
            want, got = self.behaviour(artifact, env), self.behaviour(binary, env)
            verdict = ("same", "") if want == got else ("WRONG", differ(want, got))
        if verdict[0] != "WRONG" and verdict[0] != expect:
            verdict = ("WRONG", f"expected {expect}, got {verdict[0]}: {verdict[1]}")
        # A Mach-O executable is linked in full after every edit.
        if verdict[0] == "same" and step.get("links") == "spliced" and sys.platform != "darwin":
            with open(self.daemon_log, errors="replace") as log:
                log.seek(logged)
                links = [line for line in log.read().splitlines() if line.startswith("link: ")]
            if not any("(spliced" in line or "the splice waits" in line for line in links):
                verdict = ("WRONG", f"linked in full where it is spliced: {links[-1] if links else 'no link'}")
        return (step["what"], verdict[0], verdict[1][:300])

    def mid_reload(self, step, env, daemon, ask, binary, before):
        """Catches the daemon in the middle of loading the checkout again (it waits there for
        BLAZE_RELOAD_PAUSE_MS and says so) and has clients ask meanwhile. Each runs the program
        it is answered with: that is the one from before the change or cargo's of after it."""
        log = os.path.join(self.work, "daemon.log")
        pauses = lambda: open(log).read().count("reload: paused")
        paused, deadline = pauses(), time.time() + 120
        first = subprocess.Popen(ask, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
        while pauses() == paused:
            if time.time() > deadline or daemon.poll() is not None:
                first.kill()
                return (step["what"], "WRONG", "the daemon did not stop in the middle of a reload")
            time.sleep(0.02)

        def client(n):
            said = subprocess.run(ask, capture_output=True, text=True, timeout=300, env=env)
            said = (said.stdout + said.stderr).strip()
            if not said.startswith("ok"):
                return said
            # Another client's answer may be linking over the file: taken once it holds still.
            for attempt in range(200):
                # A file of its own each time: an earlier copy may still be held by a child.
                mine = f"{binary}.client{n}.{attempt}"
                try:
                    shutil.copy(binary, mine)
                    ran = self.behaviour(mine, env)
                except OSError as busy:
                    # Another thread's child, forked while the copy was open, holds it for a moment.
                    if busy.errno != errno.ETXTBSY:
                        raise
                    time.sleep(0.05)
                    continue
                if open(mine, "rb").read() == open(binary, "rb").read():
                    return ran
                time.sleep(0.05)
            return "the program did not hold still"

        with ThreadPoolExecutor(step["clients"]) as pool:
            answers = list(pool.map(client, range(step["clients"])))
        answers.append(client("first") if first.communicate(timeout=300)[0].strip().startswith("ok") else "the first was refused")
        after = self.behaviour(self.cargo(env), env)
        if before == after:
            return (step["what"], "WRONG", "the change does not show in cargo's program")
        mixed = [it for it in answers if it not in (before, after)]
        if mixed:
            return (step["what"], "WRONG", f"{len(mixed)} of {len(answers)} clients got neither the old nor the new program: {mixed[0]!r}"[:300])
        if self.behaviour(binary, env) != after:
            return (step["what"], "WRONG", "the program is not cargo's once the reload is over")
        new = sum(it == after for it in answers)
        return (step["what"], "same", f"{len(answers)} clients mid-reload: {len(answers) - new} old, {new} new")

    def doctests(self, env):
        """`cargo test --doc` with rustdoc, and `blaze test --doc`."""
        def tested(target_dir, blaze=False):
            path = os.path.dirname(BLAZE) + os.pathsep + env.get("PATH", os.environ["PATH"])
            # Windows looks a program up in this process's PATH, not the child's.
            command = [shutil.which("blaze", path=path) or "blaze" if blaze else "cargo", "test", "--doc"]
            run = subprocess.run([*command, *self.select[:2], *self.cargo_args, "--", *self.spec.get("args", [])],
                                 cwd=self.ws, capture_output=True, text=True, timeout=900,
                                 env={**env, "PATH": path, "CARGO_TARGET_DIR": os.path.join(self.ws, target_dir)})
            return run.returncode, doctest_report(run.stdout), run.stderr
        want = tested("target-ref")
        got = tested("target", blaze=True)
        verdict, said = judge_doctests(want, got)
        for who, it in (("cargo", want), ("blaze", got)):
            print(f"DEBUG-DOCTEST {self.name} {self.backend} {who} exit {it[0]}", file=sys.stderr, flush=True)
            for l in it[1]:
                print(f"DEBUG-DOCTEST {self.name} {who} out: {l!r}", file=sys.stderr, flush=True)
            for l in it[2].splitlines()[-80:]:
                print(f"DEBUG-DOCTEST {self.name} {who} err: {l}", file=sys.stderr, flush=True)
        if verdict == "WRONG":
            raise Wrong(said)
        return ("supported", said, [])

    def row(self):
        """The case's line of the table."""
        steps = []
        if "unbuilt" in self.spec:
            return ("refused", self.spec["unbuilt"], steps)
        if self.target in CROSS and "linux" not in host():
            return ("skipped", f"{self.target} is built and run from a Linux host: its linker and runner are not of {host()}", steps)
        env = dict(self.env)
        try:
            self.prepare()
            if self.spec.get("doctests"):
                return self.doctests(env)
            artifact = self.cargo(env)
            if artifact is None and not self.spec.get("fails"):
                return ("WRONG", "the case itself does not build with cargo: " + self.cargo_error[-300:], steps)
            try:
                built, objects, compiled = self.batch(env)
            except Wrong as wrong:
                if artifact is None:
                    return ("refused", "cargo turns it down too", steps)
                raise
            if artifact is None:
                return ("WRONG", "cargo turns the project down, blaze built it", steps)
            want, got = self.behaviour(artifact, env), self.behaviour(built, env, objects)
            if want != got:
                return ("WRONG", differ(want, got), steps)
            if self.spec.get("steps"):
                steps = self.follow(env)
            return ("supported", f"{compiled} functions", steps)
        except Refused as refused:
            return ("refused", str(refused), steps)
        except Wrong as wrong:
            return ("WRONG", str(wrong), steps)
        except Exception as error:
            return ("WRONG", f"the harness failed: {error!r}"[:300], steps)
        finally:
            if not self.keep and hasattr(self, "ws"):
                shutil.rmtree(self.ws, ignore_errors=True)


def doctest_report(stdout):
    """What a run of doctests printed, without what differs from run to run: the order the
    tests finished in, how long they took, and the numbers of the threads."""
    out = re.sub(r"finished in [0-9.]+s", "finished", stdout)
    # rustdoc builds the examples of an edition 2024 crate as one program and says how long
    # that took; blaze builds each alone.
    out = re.sub(r"\nall doctests ran in [0-9.]+s; merged doctests compilation took [0-9.]+s\n", "\n", out)
    out = re.sub(r"thread '(.*?)' \(\d+\) panicked", r"thread '\1' panicked", out)
    blocks = re.split(r"\n(?=---- )", out)
    # The last failure is followed by the names of all of them and the result.
    ends = []
    if len(blocks) > 1:
        blocks[-1], _, end = blocks[-1].partition("\n\nfailures:\n")
        ends = sorted(l for l in end.splitlines() if l.startswith("    ")) + [l for l in end.splitlines() if not l.startswith("    ")]
    lines = blocks[0].splitlines()
    tests = sorted(l for l in lines if l.startswith("test ") and not l.startswith("test result"))
    rest = [l for l in lines if l not in tests]
    return tests + rest + sorted(b.strip("\n") for b in blocks[1:]) + ends


def judge_doctests(want, got):
    """What cargo's run of doctests `want` and blaze's `got` come to, each (exit status,
    [`doctest_report`], standard error): ("same", what blaze built) or ("WRONG", the first
    difference). An example blaze did not build fails saying why, and is counted rather than
    compared."""
    said = "blaze did not build the example: "
    blocks = [l for l in got[1] if l.startswith("---- ") and f" stdout ----\n{said}" in l]
    names = {l[5:l.index(" stdout ----")] for l in blocks}

    def kept(report):
        for l in report:
            if l.startswith("test result") and names:
                continue
            if l.startswith("test ") and any(l.startswith(f"test {n} ") for n in names):
                continue
            if l.startswith("---- ") and l[5:].split(" stdout ----")[0] in names:
                continue
            if l.startswith("    ") and l.strip() in names:
                continue
            if names and l in ("failures:", ""):
                continue
            yield l

    w, g = list(kept(want[1])), list(kept(got[1]))
    if w != g or (not names and want[0] != got[0]):
        lines = [f"cargo's `{a}`, blaze's `{b}`" for a, b in zip(w, g) if a != b]
        return "WRONG", (lines[0] if lines else f"cargo's ends with {want[0]} after {len(w)} lines, blaze's with {got[0]} after {len(g)}")[:300]
    built = re.findall(r"doctests: (\d+) built by blaze, (\d+) not built", got[2])
    if not built:
        return "WRONG", "blaze did not say what it built for the doctests"
    tests = sum(l.startswith("test ") and not l.startswith("test result") for l in want[1])
    return "same", f"{tests} doctests, {sum(int(n) for n, _ in built)} built by blaze, {len(names)} not built"


def differ(want, got):
    if want[0] != got[0]:
        return f"cargo's exits with {want[0]}, blaze's with {got[0]}: {got[2].decode(errors='replace').strip()[-160:]}"
    for stream in (1, 2):
        a, b = (x[stream].decode(errors="replace").splitlines() for x in (want, got))
        for w, g in zip(a, b):
            if w != g:
                return f"cargo's prints `{w[:120]}`, blaze's `{g[:120]}`"
        if len(a) != len(b):
            return f"cargo's prints {len(a)} lines, blaze's {len(b)}"
    return "differs"


def said(why):
    """A reason as the table has it: without the scratch directory, which is another each run."""
    why = re.sub(r"^(Error: )?(daemon: )?(error: )?", "", why)
    return re.sub(r"/tmp/blaze-bm-[^/ ]*/", "", why).replace("|", "/")[:240]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cases", nargs="*")
    ap.add_argument("--backend", action="append", choices=["cranelift", "llvm"])
    ap.add_argument("--table", help="write the table as Markdown")
    ap.add_argument("--keep", action="store_true", help="keep the scratch copies")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 4), help="cases built at once")
    a = ap.parse_args()
    names = a.cases or sorted(n for n in os.listdir(CASES) if os.path.exists(os.path.join(CASES, n, "case.json")))
    backends = a.backend or ["cranelift", "llvm"]
    table, bad = [], 0
    here = host().split("-")[0]
    cases = [Case(name, backend, a.keep) for name in names for backend in backends]
    for case in cases:
        if case.spec.get("on", here) != here:
            print(f"{case.name:34} {case.backend:9} not run   the case is of {case.spec['on']} hosts", flush=True)
    cases = [case for case in cases if case.spec.get("on", here) == here]
    with ThreadPoolExecutor(a.jobs) as pool:
        rows = pool.map(Case.row, cases)
        for case, (verdict, detail, steps) in zip(cases, rows):
            name, backend = case.name, case.backend
            expect = case.spec.get("expect", "supported")
            known = case.spec.get("open", {})
            if isinstance(known, dict):
                known = known.get("windows" if sys.platform == "win32" else sys.platform)
            if known:
                expect = "open"
                if verdict == "WRONG":
                    verdict, detail = "open", f"{known} ({detail})"
            unexpected = verdict not in ("WRONG", "skipped", expect)
            failed = verdict == "WRONG" or unexpected or any(s[1] == "WRONG" for s in steps)
            bad += failed
            print(f"{name:34} {backend:9} {verdict:9} {'(expected ' + expect + ') ' if unexpected else ''}{detail[:200]}", flush=True)
            for what, how, why in steps:
                print(f"{'':34} {'':9}   {how:8} {what}{': ' + why[:200] if why else ''}", flush=True)
            if a.keep and failed:
                print(f"{'':34} kept {case.ws}")
            table.append((case.spec["row"], name, backend, verdict, detail, steps))
    if a.table:
        write_table(a.table, table)
    wrong = sum(t[3] == "WRONG" or any(s[1] == "WRONG" for s in t[5]) for t in table)
    skipped = sum(t[3] == "skipped" for t in table)
    known = sum(t[3] == "open" for t in table)
    print(f"{len(table) - bad - skipped} of {len(table) - skipped} as expected, {wrong} WRONG"
          + (f", {known} open" if known else "") + (f", {skipped} skipped" if skipped else ""))
    sys.exit(1 if bad else 0)


def write_table(path, table):
    lines = ["| feature | case | backend | result | detail | invalidation |", "|---|---|---|---|---|---|"]
    by_case = {}
    for row, name, backend, verdict, detail, steps in table:
        by_case.setdefault((row, name), []).append((backend, verdict, detail, steps))
    for (row, name), runs in by_case.items():
        alike = len({(v, tuple((s[0], s[1]) for s in st)) for _, v, _, st in runs}) == 1
        for backend, verdict, detail, steps in (runs[:1] if alike else runs):
            both = "both" if alike and len(runs) > 1 else backend
            changed = "; ".join(f"{what}: {how}" + (f" ({said(why)})" if how != "same" and why else "") for what, how, why in steps)
            detail = detail if verdict != "supported" else ""
            lines.append(f"| {row} | `{name}` | {both} | {verdict} | {said(detail)} | {changed.replace('|', '/')} |")
    open(path, "w").write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
