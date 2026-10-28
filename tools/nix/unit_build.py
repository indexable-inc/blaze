#!/usr/bin/env python3
"""Per-unit build step, run inside one Nix derivation per cargo unit.

Usage: unit_build.py <spec.json>

The spec is written by tools/nix/units.nix (store paths already filled in). Two kinds:

* compile: one direct `rustc` call for one cargo unit (lib, proc-macro, bin, or a
  build script). No cargo involved.
* run: execute a compiled build script (`cargo:` protocol) and record its output
  as `$out/flags.json` plus `$out/out` (the script's OUT_DIR).

Stdlib only. Any failure names the step, the command, its exit code and the stderr tail.
"""

import json
import os
import subprocess
import sys
from pathlib import Path


def fail(step: str, cmd: list[str], code: int, stderr: str) -> "None":
    tail = "\n".join(stderr.strip().splitlines()[-40:])
    sys.exit(f"unit_build: step '{step}' failed\n  command: {' '.join(cmd)}\n  exit code: {code}\n  stderr tail:\n{tail}")


def run(step: str, cmd: list[str], **kw) -> subprocess.CompletedProcess:
    p = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if p.returncode != 0:
        # Surface stdout too: build scripts and rustc print diagnostics there.
        fail(step, cmd, p.returncode, p.stderr + p.stdout[-4000:])
    return p


def load_flags(script_out: str) -> dict:
    return json.loads((Path(script_out) / "flags.json").read_text())


def compile_unit(spec: dict) -> None:
    out = Path(os.environ["out"])
    prof = spec["profile"]
    cmd = [spec["rustc"], "--crate-name", spec["crate_name"], f"--edition={spec['edition']}", spec["src"]]
    cmd += ["--crate-type", spec["crate_type"], "--emit=link", "--error-format=short", "--color=never"]
    cmd += ["-C", f"opt-level={prof['opt_level']}", "-C", "embed-bitcode=no"]
    cmd += ["-C", f"debuginfo={prof['debuginfo']}"]
    cmd += ["-C", "debug-assertions=" + ("on" if prof["debug_assertions"] else "off")]
    if prof["overflow_checks"]:
        cmd += ["-C", "overflow-checks=on"]
    if prof["panic"] == "abort" and spec["crate_type"] in ("lib", "bin"):
        cmd += ["-C", "panic=abort"]
    if prof.get("codegen_units"):
        cmd += ["-C", f"codegen-units={prof['codegen_units']}"]
    cmd += ["-C", "linker=cc"]
    for f in spec["features"]:
        cmd += ["--cfg", f'feature="{f}"']
    if spec["cap_lints"]:
        cmd += ["--cap-lints", "allow"]
    for a, b in spec["remap"]:
        cmd += [f"--remap-path-prefix={a}={b}"]

    env = dict(os.environ)
    env.update(spec["env"])

    own = spec.get("own_script")
    if own:
        fl = load_flags(own)
        env["OUT_DIR"] = str(Path(own) / "out")
        cmd += [f"--remap-path-prefix={env['OUT_DIR']}=/out/{spec['crate_name']}"]
        for c in fl["cfgs"]:
            cmd += ["--cfg", c]
        for k, v in fl["env"].items():
            env[k] = v
        for lib in fl["link_libs"]:
            cmd += ["-l", lib]
        for s in fl["link_search"]:
            cmd += ["-L", s]
        cmd += fl["extra_flags"]
    # -L from dependencies' build scripts reaches dependents' links (cargo does the same).
    for s in spec["link_scripts"]:
        for p in load_flags(s)["link_search"]:
            cmd += ["-L", p]

    for d in spec["dep_dirs"]:
        cmd += ["-L", f"dependency={d}"]
    for e in spec["externs"]:
        cmd += ["--extern", f"{e['name']}={e['path']}"]
    if spec["crate_type"] == "proc-macro":
        cmd += ["--extern", "proc_macro"]

    if spec["crate_type"] == "bin":
        (out / "bin").mkdir(parents=True)
        cmd += ["-o", str(out / "bin" / spec["bin_name"])]
    else:
        (out / "lib").mkdir(parents=True)
        cmd += ["--out-dir", str(out / "lib"), "-C", f"metadata={spec['metadata']}", "-C", f"extra-filename=-{spec['metadata']}"]
    for extra in spec.get("link_args", []):
        cmd += ["-C", f"link-arg={extra}"]
    run("rustc " + spec["crate_name"], cmd, env=env, cwd=spec["manifest_dir"])


def cfg_env(rustc: str, prof: dict) -> dict:
    """CARGO_CFG_* from `rustc --print cfg`, as cargo sets them for build scripts."""
    p = run("rustc --print cfg", [rustc, "--print", "cfg", "-C", "debug-assertions=" + ("on" if prof["debug_assertions"] else "off")])
    vals: dict[str, list[str]] = {}
    for line in p.stdout.splitlines():
        k, _, v = line.partition("=")
        vals.setdefault(k, [])
        if v:
            vals[k].append(v.strip('"'))
    return {"CARGO_CFG_" + k.upper(): ",".join(v) for k, v in vals.items()}


def run_script(spec: dict) -> None:
    out = Path(os.environ["out"])
    out_dir = out / "out"
    out_dir.mkdir(parents=True)
    prof = spec["profile"]
    rustc = spec["rustc"]
    host = next(l.split(": ")[1] for l in run("rustc -vV", [rustc, "-vV"]).stdout.splitlines() if l.startswith("host: "))
    env = dict(os.environ)
    env.update(spec["env"])
    env.update(cfg_env(rustc, prof))
    env.update({
        "OUT_DIR": str(out_dir), "TARGET": host, "HOST": host, "RUSTC": rustc,
        "PROFILE": "release" if prof["name"] != "dev" else "debug",
        "OPT_LEVEL": str(prof["opt_level"]), "DEBUG": "true" if prof["debuginfo"] else "false",
        "NUM_JOBS": os.environ.get("NIX_BUILD_CORES", "1"), "CARGO_ENCODED_RUSTFLAGS": "",
        "CARGO_MANIFEST_DIR": spec["manifest_dir"],
    })
    for f in spec["features"]:
        env["CARGO_FEATURE_" + f.upper().replace("-", "_")] = "1"
    for d in spec["dep_scripts"]:
        for k, v in load_flags(d["path"])["metadata"].items():
            env[f"DEP_{d['links'].upper().replace('-', '_')}_{k.upper().replace('-', '_')}"] = v

    p = run("build script " + spec["pkg"], [spec["script"]], env=env, cwd=spec["manifest_dir"])
    flags = {"cfgs": [], "link_libs": [], "link_search": [], "env": {}, "metadata": {}, "extra_flags": []}
    for line in p.stdout.splitlines():
        if not line.startswith("cargo:"):
            continue
        body = line[len("cargo::") if line.startswith("cargo::") else len("cargo:"):]
        key, _, val = body.partition("=")
        if key == "rustc-cfg":
            flags["cfgs"].append(val)
        elif key == "rustc-link-lib":
            flags["link_libs"].append(val)
        elif key == "rustc-link-search":
            flags["link_search"].append(val)
        elif key == "rustc-env":
            k, _, v = val.partition("=")
            flags["env"][k] = v
        elif key == "rustc-flags":
            flags["extra_flags"] += val.split()
        elif key in ("rerun-if-changed", "rerun-if-env-changed", "warning", "rustc-check-cfg", "rustc-cdylib-link-arg", "rustc-link-arg", "rustc-link-arg-bins", "error"):
            if key.startswith("rustc-link-arg"):
                flags.setdefault("link_args", []).append(val)
        elif key == "metadata":
            k, _, v = val.partition("=")
            flags["metadata"][k] = v
        else:
            flags["metadata"][key] = val
    (out / "flags.json").write_text(json.dumps(flags, sort_keys=True))


def main() -> None:
    spec = json.loads(Path(sys.argv[1]).read_text())
    {"compile": compile_unit, "run": run_script}[spec["kind"]](spec)


if __name__ == "__main__":
    main()
