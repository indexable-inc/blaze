#!/usr/bin/env python3
"""netdown.py: a build never waits on the network (docs/cache.md, "The standard library").

The standard library a release ships is fetched from a remote over HTTP. Here that remote is
made by the blaze under test (`blaze-engine std`) and served from this process, as it is, slowly
(each answer after SLOW seconds), as a server that takes the connection and never answers
(`hang`), and as an address nothing answers at (`dead`, an unroutable one: the connection is
never made). Each build is hello world's whole program, which starts from the standard library.

  - warm: once a store has fetched it, a build makes no request at all. With the remote slow,
    hung or dead it takes as long as with it fine (within 5% or 0.3 s, best of three) and
    prints no error.
  - cold: with an empty store and the remote slow, hung or dead, the build compiles the
    standard library from the start, not after the fetch: it takes as long as a build told to
    fetch nothing (BLAZE_STD_FROM=off), within 5% or 0.5 s, where waiting would cost SLOW
    seconds or more, and says that the fetch failed in one line at most.

    netdown.py --bin target/release/blaze-engine [--toolchain nightly-YYYY-MM-DD]

rustup has to have the toolchain (harness/toolchain by default); the standard library is
blaze's library/, not its rust-src. Python 3, stdlib only.
"""
import argparse, http.server, os, re, shutil, socket, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
HELLO = 'fn main() {\n    println!("hello");\n}\n'
SLOW = 20.0
DEAD = "http://10.255.255.1:9"
COMPILED = re.compile(r"^standard library: \d+ crates, compiled", re.M)
FETCHED = re.compile(r"^standard library: .* from http\S* in ", re.M)
FAILED = re.compile(r"^standard library: not fetched", re.M)


def serve(root, mode):
    """The address of an HTTP server of the files in `root` (`/` as `.`), answering as `mode`."""
    if mode == "hang":
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(64)
        taken = []
        threading.Thread(target=lambda: [taken.append(listener.accept()) for _ in iter(int, 1)],
                         daemon=True).start()
        return f"http://127.0.0.1:{listener.getsockname()[1]}"

    class Files(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=root, **kwargs)

        def send_head(self):
            if mode == "slow":
                time.sleep(SLOW)
            return super().send_head()

        def log_message(self, *_):
            pass

    class Server(http.server.ThreadingHTTPServer):
        # A build that is done hangs up on an answer still coming: not this script's error.
        def handle_error(self, *_):
            pass

    server = Server(("127.0.0.1", 0), Files)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}"


class Builds:
    def __init__(self, binary, sysroot, work):
        self.binary, self.sysroot, self.work = binary, sysroot, work
        self.project = os.path.join(work, "p")
        os.makedirs(os.path.join(self.project, "src"))
        open(os.path.join(self.project, "Cargo.toml"), "w").write(
            '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n')
        open(os.path.join(self.project, "src/main.rs"), "w").write(HELLO)
        self.runs = 0

    def env(self, store, std_from):
        env = {k: v for k, v in os.environ.items()
               if k not in ("RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR") and not k.startswith("BLAZE_")}
        self.runs += 1
        env.update(PATH=os.path.join(self.sysroot, "bin") + os.pathsep + env.get("PATH", ""),
                   BLAZE_CACHE=store, BLAZE_HOME=os.path.join(self.work, f"home-{self.runs}"),
                   BLAZE_STD_FROM=std_from, RUST_BACKTRACE="0")
        return env

    def build(self, store, std_from):
        """(seconds, stderr) of one build of hello world's whole program."""
        out = os.path.join(self.work, f"out-{self.runs}")
        os.makedirs(out, exist_ok=True)
        began = time.monotonic()
        done = subprocess.run(
            [self.binary, self.project, "--own-target", "--profile", "dev", "--crate", "src/main.rs",
             "--out", os.path.join(out, "x.o"), "--exe", os.path.join(out, "hello")],
            cwd=self.project, env=self.env(store, std_from), capture_output=True, text=True,
            timeout=1800)
        took = time.monotonic() - began
        if done.returncode != 0:
            tail = "\n".join(done.stderr.splitlines()[-30:])
            sys.exit(f"error: netdown: the build failed (exit {done.returncode}):\n{tail}")
        ran = subprocess.run([os.path.join(out, "hello")], capture_output=True, text=True)
        if ran.stdout != "hello\n":
            sys.exit(f"error: netdown: the program printed {ran.stdout!r}")
        return took, done.stderr

    def best(self, store, std_from, rounds=3):
        runs = [self.build(store, std_from) for _ in range(rounds)]
        return min(it[0] for it in runs), [it[1] for it in runs]


def within(took, base, share, slack):
    return took <= base * (1 + share) + slack


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bin", required=True)
    ap.add_argument("--toolchain", default=open(os.path.join(HERE, "toolchain")).read().strip())
    args = ap.parse_args()
    binary = os.path.abspath(args.bin)
    sysroot = subprocess.run(["rustc", f"+{args.toolchain}", "--print", "sysroot"],
                             capture_output=True, text=True, check=True).stdout.strip()
    failures = []
    with tempfile.TemporaryDirectory(prefix="netdown-") as work:
        builds = Builds(binary, sysroot, work)
        remote = os.path.join(work, "remote")
        made = subprocess.run([binary, "std", remote, sysroot], env=builds.env(os.path.join(work, "std-store"), "off"),
                              capture_output=True, text=True, timeout=3600)
        if made.returncode != 0:
            sys.exit(f"error: netdown: blaze-engine std failed:\n{made.stderr[-3000:]}")
        served = os.path.join(work, "served")
        os.makedirs(served)
        for dir, _, files in os.walk(remote):
            for name in files:
                path = os.path.relpath(os.path.join(dir, name), remote)
                shutil.copy(os.path.join(dir, name), os.path.join(served, path.replace(os.sep, ".")))
        remotes = {mode: serve(served, mode) for mode in ("ok", "slow", "hang")}
        remotes["dead"] = DEAD

        # warm
        store = os.path.join(work, "warm-store")
        _, said = builds.build(store, remotes["ok"])
        if not FETCHED.search(said):
            sys.exit(f"error: netdown: the first build fetched nothing from {remotes['ok']}:\n{said[-3000:]}")
        base, _ = builds.best(store, remotes["ok"])
        print(f"warm, remote ok: {base:.2f} s")
        for mode in ("slow", "hang", "dead"):
            took, said = builds.best(store, remotes[mode])
            errors = [l for l in "".join(said).splitlines() if re.search(r"error|not fetched|failed", l, re.I)]
            fine = within(took, base, 0.05, 0.3) and not errors
            print(f"warm, remote {mode}: {took:.2f} s{'' if fine else '  FAILED'}")
            if not fine:
                failures.append(f"warm {mode}: {took:.2f} s against {base:.2f} s; {errors[:3]}")

        # cold
        local, _ = builds.build(os.path.join(work, "cold-off"), "off")
        print(f"cold, nothing fetched: {local:.2f} s")
        for mode in ("slow", "hang", "dead"):
            took, said = builds.build(os.path.join(work, f"cold-{mode}"), remotes[mode])
            lines = FAILED.findall(said)
            fine = within(took, local, 0.05, 0.5) and len(lines) <= 1 and COMPILED.search(said)
            print(f"cold, remote {mode}: {took:.2f} s{'' if fine else '  FAILED'}")
            if not fine:
                failures.append(f"cold {mode}: {took:.2f} s against {local:.2f} s, {len(lines)} failure lines")
    if failures:
        sys.exit("error: netdown:\n  " + "\n  ".join(failures))
    print("netdown: no build waited on the network")


if __name__ == "__main__":
    main()
