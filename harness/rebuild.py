"""rebuild.py: an edit gives the same program as a fresh build.

What programs.py, includes.py, unwinds.py, stress.py, worktrees.py, keys.py and fuzz/edits.py
share. A `Project` is a checkout with a home of its own and the daemon of it
(`storelib.Daemon`): a harness edits the checkout, asks the daemon for a program with `blaze
build`, and `check` holds that program to the one `blaze build` gives of the same source in an
empty home (`fresh`). The two have to end with the same status and print the same, or both
builds have to be refused. The fresh build is of the same checkout, with a target directory of
its own outside it, so `CARGO_MANIFEST_DIR` and the paths a panic names are the same in both;
it is made once per state of the checkout.

A verdict is a word, upper case when it is wrong: `same`, `refused` (both builds were), `REFUSED`
(the warm build only), `FRESH REFUSED` (the fresh build only), `BUILT BROKEN CODE` (a step the
harness wrote as not Rust built), `DIFFERENT OUTPUT`, `DAEMON DIED`.

BLAZE_BIN names the blaze under test (blaze_cli.BLAZE).
"""
import hashlib, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import BLAZE
from storelib import BUILD_SECS, RUN_SECS, RUNNER, Daemon, Failed, scratch  # noqa: F401 (scratch: for the harnesses)

WRONG = ("REFUSED", "FRESH REFUSED", "BUILT BROKEN CODE", "DIFFERENT OUTPUT", "DAEMON DIED")


def edit(ws, path, old, new):
    """`old` replaced by `new` once in `ws/path`; `old` None writes the file whole, `new` None
    removes it."""
    at = os.path.join(ws, path)
    if new is None:
        os.remove(at)
        return
    if old is None:
        os.makedirs(os.path.dirname(at), exist_ok=True)
        open(at, "w").write(new)
        return
    text = open(at).read()
    if old not in text:
        raise Failed(f"{at} has no {old!r}")
    open(at, "w").write(text.replace(old, new, 1))


def copy(source, to):
    """A copy of the workspace at `source`, without what was built in it."""
    shutil.copytree(source, to, ignore=shutil.ignore_patterns("target", "target-*"))
    return to


def tree(ws):
    """A hash of every file of the checkout but what is built in it: two equal are one source."""
    digest = hashlib.blake2b(digest_size=16)
    for root, dirs, names in os.walk(ws):
        dirs[:] = sorted(it for it in dirs if it != "target" and not it.startswith("target-"))
        for name in sorted(names):
            path = os.path.join(root, name)
            digest.update(os.path.relpath(path, ws).encode() + b"\0")
            with open(path, "rb") as file:
                digest.update(file.read() + b"\0")
    return digest.hexdigest()


def behaviour(executable, cwd, targets):
    """(status, stdout, stderr) of a run of the program, the target directories it was built in
    named alike and a panic's thread id left out."""
    env = dict(os.environ, RUST_BACKTRACE="0")
    try:
        ran = subprocess.run([*RUNNER, executable], cwd=cwd, env=env, capture_output=True, timeout=RUN_SECS)
    except subprocess.TimeoutExpired:
        return "timeout", "", ""

    def clean(raw):
        text = raw.decode(errors="replace")
        for target in targets:
            text = text.replace(target, "<target>")
        return re.sub(r"' \(\d+\) panicked", "' panicked", text)

    return ran.returncode, clean(ran.stdout), clean(ran.stderr)


class Project:
    """The checkout `ws`, built by a daemon in `home` (by default `<work>/home`) with `env` set
    for it, and from nothing in homes under `work`. With `serves`, another Project, it is built
    by that one's daemon: one daemon of one home for several checkouts."""

    def __init__(self, ws, work, *, env=None, home=None, name="warm", serves=None):
        self.ws, self.work, self.name, self.serves = ws, work, name, serves
        self.env = dict(env or {})
        self.home = serves.home if serves else home or os.path.join(work, "home")
        self.target = os.path.join(ws, "target")
        self.daemon = None
        self.died = False
        self.fresh_runs = {}
        self.fresh_count = 0
        os.makedirs(work, exist_ok=True)

    def started(self):
        """The daemon, started when there is none; one that ended unasked is `died`."""
        if self.serves is not None:
            self.daemon = self.serves.started()
            self.died = self.serves.died
            return self.daemon
        if self.daemon is not None and not self.daemon.alive():
            self.died = True
            self.daemon = None
        if self.daemon is None:
            self.daemon = Daemon(BLAZE, self.home, name=self.name, env=self.env)
            if not self.daemon.listening:
                raise Failed(f"the daemon of {self.ws} did not start:\n{self.daemon.log()[-2000:]}\nlog: {self.daemon.log_path}")
        return self.daemon

    def stop(self):
        """`blaze stop`: the next build starts a daemon on the store this one filled."""
        if self.serves is not None:
            self.serves.stop()
            self.daemon = None
        elif self.daemon is not None:
            self.daemon.stop()
            self.daemon = None

    def asking(self, program, *args, command="build"):
        """A `blaze COMMAND` of `program` started and not waited for."""
        daemon = self.started()
        return subprocess.Popen([BLAZE, command, "--message-format=json", *args], cwd=self.ws, env=daemon.env,
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def answer(self, client, program):
        """What the build started by `asking` did (`storelib.Built`)."""
        return self.daemon.answer(client, program, BUILD_SECS)

    def build(self, program, *args, command="build"):
        return self.answer(self.asking(program, *args, command=command), program)

    def fresh(self, program, *args, command="build"):
        """What the program `blaze COMMAND ARGS` makes of the checkout as it is now, with `env`
        as it is now, does when built in an empty home: `behaviour`, or None when the build is
        refused."""
        key = (program, command, args, tuple(sorted(self.env.items())), tree(self.ws))
        if key not in self.fresh_runs:
            self.fresh_count += 1
            at = os.path.join(self.work, f"fresh-{self.fresh_count}")
            target = os.path.join(at, "target")
            daemon = Daemon(BLAZE, os.path.join(at, "home"), env={**self.env, "CARGO_TARGET_DIR": target})
            if not daemon.listening:
                raise Failed(f"the daemon of a fresh build did not start:\n{daemon.log()[-2000:]}\nlog: {daemon.log_path}")
            try:
                client = subprocess.Popen([BLAZE, command, "--message-format=json", *args], cwd=self.ws, env=daemon.env,
                                          stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                built = daemon.answer(client, program, BUILD_SECS)
                self.fresh_runs[key] = (behaviour(built.executable, self.ws, (target, self.target))
                                        if built.status == 0 and built.executable else None)
            finally:
                daemon.stop()
            shutil.rmtree(at, ignore_errors=True)
        return self.fresh_runs[key]

    def ran(self, built):
        return behaviour(built.executable, self.ws, (self.target,))

    def check(self, program, *args, broken=False, command="build"):
        """The daemon's build of `program` held to a fresh one: (verdict, what it said)."""
        built = self.build(program, *args, command=command)
        return self.held(built, program, *args, broken=broken, command=command)

    def held(self, built, program, *args, broken=False, command="build"):
        """A build the daemon has done, held to a fresh one of the same checkout."""
        if self.daemon is None or not self.daemon.alive():
            self.died = True
            return "DAEMON DIED", built.said[-2000:]
        warm = self.ran(built) if built.status == 0 and built.executable else None
        cold = self.fresh(program, *args, command=command)
        if warm is None and cold is None:
            return "refused", last_error(built.said)
        if warm is None:
            return "REFUSED", built.said[-2000:]
        if cold is None:
            return "FRESH REFUSED", f"the daemon's program: {warm}"
        if broken:
            return "BUILT BROKEN CODE", f"{warm}"
        if warm != cold:
            return "DIFFERENT OUTPUT", f"after the edit: {warm}\nfresh: {cold}"
        return "same", ""


def last_error(said):
    """The first line of the last error a refused build gave."""
    errors = [line for line in said.splitlines() if line.startswith("error")]
    return errors[-1][:160] if errors else said.strip()[-160:]
