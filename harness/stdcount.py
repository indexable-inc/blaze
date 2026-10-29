#!/usr/bin/env python3
"""stdcount.py: how many of the standard library's functions a build of blaze refuses, and
whether that went up against another build. A toolchain bump must not make more of std rustc's
(docs/toolchain.md, "The daily bump").

    stdcount.py --bin blaze-engine --toolchain nightly-YYYY-MM-DD [--json out.json]
    stdcount.py --bin new/blaze --toolchain nightly-B --base old/blaze --base-toolchain nightly-A

Each build compiles hello world's whole program with an empty store and BLAZE_STD_FROM=off, so
the standard library of its own library/ (the checkout it was built in has it, docs/toolchain.md)
is compiled in the process, and reads the line `standard library: N crates, compiled C of T
fns`: refused is T - C, and the reasons are the counted lines the build prints for them. With
--base the second build is measured the same way, and the run fails when the first refuses more
than the base. rustup has to have the toolchains. Python 3, stdlib only.
"""
import argparse, collections, json, os, re, subprocess, sys, tempfile

LINE = re.compile(r"standard library: (\d+) crates, compiled (\d+) of (\d+) fns")
HELLO = 'fn main() {\n    println!("hello");\n}\n'


def sysroot(toolchain):
    return subprocess.run(["rustc", f"+{toolchain}", "--print", "sysroot"], capture_output=True,
                          text=True, check=True).stdout.strip()


def measure(binary, toolchain, keep=None):
    """{crates, compiled, total, refused, reasons} of one build of hello world by `binary`."""
    root = sysroot(toolchain)
    with tempfile.TemporaryDirectory(prefix="stdcount-") as work:
        project = os.path.join(work, "p")
        os.makedirs(os.path.join(project, "src"))
        open(os.path.join(project, "Cargo.toml"), "w").write(
            '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n')
        open(os.path.join(project, "src/main.rs"), "w").write(HELLO)
        env = {k: v for k, v in os.environ.items()
               if k not in ("RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR") and not k.startswith("BLAZE_")}
        env.update(PATH=os.path.join(root, "bin") + os.pathsep + env.get("PATH", ""),
                   BLAZE_CACHE=os.path.join(work, "store"), BLAZE_HOME=os.path.join(work, "home"),
                   BLAZE_STD_FROM="off", RUST_BACKTRACE="0")
        done = subprocess.run(
            [binary, project, "--own-target", "--profile", "release", "--crate", "src/main.rs",
             "--out", os.path.join(project, "x.o"), "--exe", os.path.join(project, "hello")],
            cwd=project, env=env, capture_output=True, text=True, timeout=3600)
    if keep:
        open(keep, "w").write(done.stderr)
    found = LINE.search(done.stderr)
    if not found:
        tail = "\n".join(done.stderr.splitlines()[-30:])
        sys.exit(f"error: stdcount: {binary} with {toolchain} printed no `standard library: ... compiled C of T fns` "
                 f"line (exit {done.returncode}):\n{tail}")
    crates, compiled, total = map(int, found.groups())
    after = done.stderr[found.end():]
    reasons = collections.Counter()
    # main.rs `report`: `  {count:>5}  {reason}  (e.g. {example})`, up to the program's own line.
    for line in after.splitlines():
        counted = re.match(r"\s+(\d+)  (.+?)  \(e\.g\. ", line)
        if counted:
            reasons[counted.group(2)[:160]] += int(counted.group(1))
        elif not line.startswith(" "):
            break
    return {"toolchain": toolchain, "crates": crates, "compiled": compiled, "total": total,
            "refused": total - compiled, "reasons": dict(reasons.most_common(40))}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bin", required=True)
    ap.add_argument("--toolchain", required=True)
    ap.add_argument("--base", help="the build to compare with (main's)")
    ap.add_argument("--base-toolchain")
    ap.add_argument("--json", help="write both measurements here")
    ap.add_argument("--stderr", help="keep the build's stderr in this file")
    args = ap.parse_args()
    got = measure(args.bin, args.toolchain, args.stderr)
    out = {"new": got}
    print(f"{args.toolchain}: {got['crates']} crates, compiled {got['compiled']} of {got['total']} fns, "
          f"refused {got['refused']}")
    worse = False
    if args.base:
        base = measure(args.base, args.base_toolchain or args.toolchain)
        out["base"] = base
        print(f"{base['toolchain']} (base): compiled {base['compiled']} of {base['total']} fns, refused {base['refused']}")
        worse = got["refused"] > base["refused"]
        print(f"refused {got['refused'] - base['refused']:+d}: " + ("MORE than the base" if worse else "not more than the base"))
    if args.json:
        json.dump(out, open(args.json, "w"), indent=1)
    sys.exit(1 if worse else 0)


if __name__ == "__main__":
    main()
