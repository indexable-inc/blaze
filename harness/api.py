#!/usr/bin/env python3
"""api.py: the agent API of the daemon (crates/ra-jit/src/cargo/daemon/api.rs), end to end.

usage: api.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A toy workspace is built and loaded, and then, each checked against what cargo builds of the
same source:

  frames     `cargo ra-jit build` asks for its build as a call (the daemon's home has
             `daemon.api`), and the build is the daemon's
  apply      `cargo ra-jit apply --build PATH=FROM` writes the file, builds it and says the root
             of the worker's sources; `cargo ra-jit build` after it is the daemon's and places a
             program that runs the new code
  memory     `--memory` changes the root and leaves the disk alone
  new, gone  a module and the `mod` that names it in one transaction, built, then both undone:
             the program runs each, and the root is back to what it was
  refused    a file of no workspace loaded, two workspaces in one call, a call of another
             schema and bytes that are no deterministic CBOR are each refused, with why
  pipelined  on one connection, a Hello sent after an Apply that builds is answered first
  http       RA_JIT_HTTP: refused without the token, answered as JSON with it

The calls of this file are encoded by cbor.py, a codec of the harness's own, which is a second
reading of the encoding: its bytes have to be what the daemon decodes.

Python 3, stdlib only.
"""
import glob, json, os, re, shutil, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, wait_warm
from cbor import SCHEMA, call, encode, framed, read_frame
FILES = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/main.rs": "".join(f"pub fn part_{i}() -> u32 {{\n    {i}\n}}\n\n" for i in range(8))
                   + 'fn main() {\n    println!("{}", part_3() + part_5());\n}\n',
}


def tails(home, lines=40):
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-api-", dir="/tmp"))
    ws, other, home = os.path.join(root, "ws"), os.path.join(root, "other"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    os.makedirs(other)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    env["RA_JIT_HTTP"] = "127.0.0.1:0"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    program, main_rs = os.path.join(ws, "target", "debug", "app"), os.path.join(ws, "src", "main.rs")
    wrong = []

    def check(step, right, detail=""):
        print(f"{step:30} {'ok' if right else 'WRONG':8} {detail}")
        if not right:
            wrong.append(f"{step}: {detail}")

    def apply(*args):
        return subprocess.run(["cargo", "ra-jit", "apply", *args], cwd=ws, env=env, capture_output=True, text=True, timeout=600)

    def root_of(ran):
        found = re.search(r"^root ([0-9a-f]{64})$", ran.stdout, re.M)
        return found and found[1]

    try:
        _, built = side.cargo("build", "--bin", "app")
        if built.returncode != 0:
            raise SystemExit(f"the cold build failed:\n{(built.stdout + built.stderr)[-3000:]}{tails(home)}")
        took, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        check("marker", os.path.exists(os.path.join(home, "daemon.api")), "daemon.api in the home")
        _, built = side.cargo("build", "--bin", "app")
        check("frames", "[ra-jit]" in built.stdout + built.stderr, "the build through a call is the daemon's")

        def placed(step, ran, root_right, took):
            """`cargo ra-jit build` after the transaction places what it built, as cargo would."""
            began = time.monotonic()
            _, built = side.cargo("build", "--bin", "app")
            placing = time.monotonic() - began
            out = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout
            _, theirs = reference.cargo("run", "-q", "--bin", "app")
            right = ran.returncode == 0 and root_right(root_of(ran)) and "[ra-jit]" in built.stdout + built.stderr
            check(step, right and out == theirs.stdout and ran.stdout.count("\nok ") == 1,
                  f"apply {took * 1000:.0f} ms, then build {placing * 1000:.0f} ms; printed {out.strip()!r}, "
                  f"cargo {theirs.stdout.strip()!r}: {ran.stdout.strip()[-200:]!r} {ran.stderr.strip()[-300:]!r}")

        def timed_apply(*args):
            began = time.monotonic()
            ran = apply(*args)
            return ran, time.monotonic() - began

        edited = os.path.join(root, "edited.rs")
        open(edited, "w").write(FILES["src/main.rs"].replace("    5\n", "    50\n"))
        ran, took = timed_apply("--build", f"src/main.rs={edited}")
        first = root_of(ran)
        placed("apply", ran, lambda it: it is not None and "50" in open(main_rs).read(), took)

        memory = os.path.join(root, "memory.rs")
        open(memory, "w").write(FILES["src/main.rs"].replace("    5\n", "    500\n"))
        ran = apply("--memory", f"src/main.rs={memory}")
        check("memory", ran.returncode == 0 and root_of(ran) not in (None, first) and "500" not in open(main_rs).read(),
              f"{ran.stdout.strip()!r} {ran.stderr.strip()[-300:]!r}")
        ran = apply("src/main.rs")
        check("memory undone", root_of(ran) == first, f"the root of the disk's text again: {ran.stdout.strip()!r}")

        extra = os.path.join(ws, "src", "extra.rs")
        made, using = os.path.join(root, "extra.rs"), os.path.join(root, "using.rs")
        open(made, "w").write("pub fn extra() -> u32 {\n    7000\n}\n")
        open(using, "w").write("mod extra;\n" + open(edited).read().replace("part_3() + part_5()", "part_3() + part_5() + extra::extra()"))
        ran, took = timed_apply("--build", f"src/main.rs={using}", f"src/extra.rs={made}")
        placed("new", ran, lambda it: it not in (None, first) and os.path.exists(extra), took)
        ran, took = timed_apply("--build", f"src/main.rs={edited}", "src/extra.rs=")
        placed("gone", ran, lambda it: it == first and not os.path.exists(extra), took)
        ran = apply("--memory", f"src/extra.rs={made}")
        check("refused: --memory new", ran.returncode == 1 and "no source" in ran.stderr, ran.stderr.strip()[-300:])

        open(os.path.join(other, "x.rs"), "w").write("\n")
        ran = apply(os.path.join(other, "x.rs"))
        check("refused: no workspace", ran.returncode == 1 and "no workspace loaded" in ran.stderr, ran.stderr.strip()[-300:])
        ran = apply("src/main.rs", os.path.join(other, "x.rs"))
        check("refused: two workspaces", ran.returncode == 1 and "workspace" in ran.stderr, ran.stderr.strip()[-300:])
        ran = apply("--json", "src/main.rs")
        try:
            said = json.loads(ran.stdout)
        except ValueError:
            said = {}
        check("json", "Applied" in said.get("body", {}), ran.stdout.strip()[-300:])

        sock = os.path.join(home, "daemon.sock")
        with socket.socket(socket.AF_UNIX) as conn:
            conn.connect(sock)
            conn.settimeout(600)
            conn.sendall(framed(encode({"id": 1, "schema": SCHEMA + 1, "body": "Hello"})))
            answer = read_frame(conn)
            check("refused: schema", "Refused" in answer["body"], repr(answer)[-200:])
            # A map with its keys out of order.
            conn.sendall(framed(b"\xa2\x62id\x01\x61b\x00"))
            answer = read_frame(conn)
            check("refused: not deterministic", "out of order" in answer["body"].get("Refused", ""), repr(answer)[-200:])
            text = FILES["src/main.rs"].replace("    5\n", "    51\n").encode()
            began = time.monotonic()
            call(conn, 2, {"Apply": {"files": [{"path": main_rs, "bytes": text}], "build": True, "disk": True}})
            call(conn, 3, "Hello")
            answers = [read_frame(conn), read_frame(conn)]
            took = time.monotonic() - began
            ids = [it["id"] for it in answers]
            applied = answers[1]["body"].get("Applied", {}) if ids == [3, 2] else {}
            check("pipelined", ids == [3, 2] and isinstance(applied.get("root"), bytes) and len(applied["root"]) == 32,
                  f"answered in the order {ids}, {took * 1000:.0f} ms: {repr(answers)[-300:]}")

        log = open(os.path.join(home, "daemon.log"), errors="replace").read()
        found = re.search(r"takes calls over HTTP at (http://\S+)", log)
        if not found:
            check("http", False, "the daemon's log says no address")
        else:
            url, token = found[1], open(os.path.join(home, "http-token")).read().strip()
            hello = encode({"id": 7, "schema": SCHEMA, "body": "Hello"})
            try:
                urllib.request.urlopen(urllib.request.Request(url, data=hello, method="POST"), timeout=60)
                check("http: no token", False, "answered")
            except urllib.error.HTTPError as error:
                check("http: no token", error.code == 401, f"{error.code}")
            asked = urllib.request.Request(url, data=hello, method="POST", headers={
                "Authorization": f"Bearer {token}", "Accept": "application/json", "Content-Type": "application/cbor"})
            said = json.loads(urllib.request.urlopen(asked, timeout=60).read())
            check("http", said.get("id") == 7 and "Hello" in said.get("body", {}), json.dumps(said)[-200:])
    finally:
        side.cargo("stop")
    check("marker gone", not os.path.exists(os.path.join(home, "daemon.api")), "daemon.api once the daemon ended")
    for why in wrong:
        print(f"  {why}")
    if wrong:
        print(f"under {root}{tails(home)}")
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
