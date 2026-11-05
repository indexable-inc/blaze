#!/usr/bin/env python3
"""scriptenv.py: a build script is given the build's environment, the command's and not the
daemon's (#720).

The script of `REPRO` is cef-dll-sys's shape: it says it reads `SDK_DIR` and fails without it
(where cef-dll-sys would download, which the sandbox refuses). Under cargo, `SDK_DIR=... cargo
run` prints the value. Under blaze it has to as well:
  fresh     with no daemon, `SDK_DIR=/opt/sdk` on the command: the run without it fails, and
            the script runs again given it; then a command without it, to the daemon that
            has it, fails as cargo's would
  later     with a daemon started by a command without `SDK_DIR`, then one with it, one with
            another value and one without: the variable is the command's, and the run of
            each value is its own
  named     the same with `build-script-env = ["SDK_DIR"]` in the root manifest
  path dep  a path dependency that is a workspace of its own is given what the build's root
            manifest names in `build-script-env`, for a script that does not say it reads it
BLAZE_BIN names the compiler, with blaze beside it.
"""
import os, shutil, subprocess, tempfile

from blaze_cli import base_env
from storelib import daemon_pid

REPRO = {
    "Cargo.toml": '[package]\nname = "script-env-repro"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n',
    "build.rs": 'fn main() {\n    println!("cargo:rerun-if-env-changed=SDK_DIR");\n'
                '    let dir = std::env::var("SDK_DIR").expect("SDK_DIR is not set, and there is no network to download the SDK");\n'
                '    println!("cargo:rustc-env=SDK_DIR={dir}");\n}\n',
    "src/main.rs": 'fn main() {\n    println!("{}", env!("SDK_DIR"));\n}\n',
}

NAMED = '\n[workspace.metadata.blaze.sandbox]\nbuild-script-env = ["SDK_DIR"]\n'

# The dependency's script says nothing of what it reads: only `build-script-env` gives it
# `VENDOR_DIR`, and the build's root manifest is the one that says so.
PATH_DEP = {
    "Cargo.toml": '[package]\nname = "uses-vendor"\nversion = "0.1.0"\nedition = "2021"\n\n'
                  '[dependencies]\nvendor = { path = "deps/vendor/vendor" }\n\n'
                  '[workspace]\n\n[workspace.metadata.blaze.sandbox]\nbuild-script-env = ["VENDOR_DIR"]\n',
    "src/main.rs": 'fn main() {\n    println!("{}", vendor::DIR);\n}\n',
    "deps/vendor/Cargo.toml": '[workspace]\nmembers = ["vendor"]\nresolver = "2"\n',
    "deps/vendor/vendor/Cargo.toml": '[package]\nname = "vendor"\nversion = "0.1.0"\nedition = "2021"\n',
    "deps/vendor/vendor/build.rs": 'fn main() {\n    let dir = std::env::var("VENDOR_DIR").expect("VENDOR_DIR is not set");\n'
                                   '    println!("cargo:rustc-env=VENDOR_DIR={dir}");\n}\n',
    "deps/vendor/vendor/src/lib.rs": 'pub const DIR: &str = env!("VENDOR_DIR");\n',
}


def written(ws, files):
    shutil.rmtree(ws, ignore_errors=True)
    for name, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, name)), exist_ok=True)
        with open(os.path.join(ws, name), "w") as out:
            out.write(text)


def main():
    work = tempfile.mkdtemp(prefix="rj-scriptenv-", dir="/tmp" if os.name != "nt" else None)
    print(f"work: {work}", flush=True)
    ws = base = None
    failed = []

    def case(name, files):
        nonlocal ws, base
        if ws:
            blaze("stop")
        ws = os.path.join(work, name)
        base = base_env(os.path.join(work, name + "-home"))
        base.pop("SDK_DIR", None)
        base.pop("VENDOR_DIR", None)
        written(ws, files)
        locked = blaze("lock")
        if locked.returncode != 0:
            raise SystemExit(f"FAIL {name}: `blaze lock` in {ws} exited {locked.returncode}: "
                             f"{locked.stderr.strip()[-1500:]}")

    def blaze(*args, **env):
        ran = subprocess.run(["blaze", *args], cwd=ws, env=dict(base, **env),
                             capture_output=True, text=True, timeout=600)
        return ran

    def daemon():
        """The daemon's pid, from its lock (`blaze status` has no machine format)."""
        return daemon_pid(base["BLAZE_HOME"])

    # Without the variable the run fails as the script's (the panic itself is in the daemon's log).
    def check(label, ran, printed=None):
        held = ran.returncode == 0 and ran.stdout == printed if printed else \
            ran.returncode != 0 and not ran.stdout and "build script" in ran.stderr
        print(f"{'PASS' if held else 'FAIL'} {label}" + ("" if held else f": exit {ran.returncode}, {ran.stdout!r}, {ran.stderr.strip()[-800:]}"), flush=True)
        if not held:
            failed.append(label)

    def fresh():
        blaze("stop")
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)

    try:
        for named in (False, True):
            case("named" if named else "said", dict(REPRO, **{"Cargo.toml": REPRO["Cargo.toml"] + (NAMED if named else "")}))
            how = "with build-script-env" if named else "said by the script"
            check(f"{how}: fresh daemon, SDK_DIR on the command", blaze("run", "-q", SDK_DIR="/opt/sdk"), "/opt/sdk\n")
            check(f"{how}: then without it, the daemon's is not taken", blaze("run", "-q"))
            fresh()
            check(f"{how}: a daemon started without SDK_DIR fails as cargo does", blaze("run", "-q"))
            pid = daemon()
            check(f"{how}: then SDK_DIR on the command", blaze("run", "-q", SDK_DIR="/opt/sdk"), "/opt/sdk\n")
            check(f"{how}: then another SDK_DIR", blaze("run", "-q", SDK_DIR="/opt/other"), "/opt/other\n")
            check(f"{how}: then without it", blaze("run", "-q"))
            check(f"{how}: and the first value again", blaze("run", "-q", SDK_DIR="/opt/sdk"), "/opt/sdk\n")
            if daemon() != pid:
                print(f"FAIL {how}: the daemon was another", flush=True)
                failed.append(f"{how}: same daemon")
        case("path-dep", PATH_DEP)
        check("path dependency in a workspace of its own: the root manifest's build-script-env",
              blaze("run", "-q", VENDOR_DIR="/opt/vendor"), "/opt/vendor\n")
    finally:
        if ws:
            blaze("stop")
    shutil.rmtree(work, ignore_errors=True)
    print("PASS" if not failed else "FAIL: " + "; ".join(failed))
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
