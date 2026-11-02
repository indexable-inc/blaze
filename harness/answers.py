#!/usr/bin/env python3
"""answers.py: writes what the pinned rustc answers to the questions blaze answers in its place
(`-vV`, `--print cfg`, cargo's probe of file names, `target-spec-json` and the rest) into blaze's
data, after a toolchain bump (docs/no-rustc.md, step B). blaze answers them from that data and
runs no rustc.

    answers.py --write   write what rustc answers into blaze's data

The data is crates/blaze-units/src/stdlib/rustc.json, of the toolchain in harness/toolchain,
which blaze's target table reads. The target specs depend on the host rustc was built for (x86-64 Linux's links with its
own lld): --write keeps those of the other hosts and writes this host's, so it is run on each
host CI has (Linux x86-64 and AArch64, macOS arm64). On a host not written (Windows) blaze
answers the spec all written hosts print, and has no answer of one they differ on.
"""
import concurrent.futures, json, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
DATA = os.path.join(ROOT, "crates/blaze-units/src/stdlib", "rustc.json")
TARGETS = [
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc", "x86_64-pc-windows-gnu",
    "wasm32-unknown-unknown", "wasm32-wasip1", "wasm32-wasip2",
]
CRATE_TYPES = ["bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"]
# What changes the cfgs rustc prints beyond the target's own; blaze adds these itself.
GIVEN = ("debug_assertions", "overflow_checks", "ub_checks", "panic=")
# A feature no target has, of whose warning blaze keeps rustc's text.
UNLISTED = "blazeunlisted"


def rustc():
    sysroot = subprocess.run(["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"], capture_output=True,
                             text=True, check=True).stdout.strip()
    return os.path.join(sysroot, "bin", "rustc")


def env(extra):
    clean = {k: v for k, v in os.environ.items()
             if not k.startswith(("RUSTC", "BLAZE_", "MACOSX_", "IPHONEOS_", "SDKROOT"))}
    clean.pop("RUST_TARGET_PATH", None)
    return {**clean, **extra}


def ask(argv, extra=None, stdin=""):
    ran = subprocess.run(argv, capture_output=True, text=True, input=stdin, env=env(extra or {}))
    return ran.returncode, ran.stdout, ran.stderr


def said(rust, args, extra=None, stdin=""):
    code, out, err = ask([rust, *args], extra, stdin)
    assert code == 0 and not err.strip(), (args, code, err)
    return out


def target_features(printed):
    """The `target_feature` cfgs of what `--print cfg` printed, sorted."""
    return sorted(line[len('target_feature="'):-1] for line in printed.splitlines() if line.startswith('target_feature="'))


def features_of(rust, t, base):
    """What `-C target-cpu` and `-C target-feature` do to a target's `target_feature` cfgs, on the
    nightly: each CPU's, by the index of the set it has; each feature rustc names (+f, without a
    warning) and each feature only LLVM names (for `target-cpu=native`'s host features), by those
    it turns on beside the target's own. A CPU or feature rustc warns of is left out: blaze refuses it."""
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    listed = ask([rust, "--print", "target-features", *t], nightly)[1]
    rustcs, llvms, section = [], [], None
    for line in listed.splitlines():
        if line.startswith("Features supported by rustc"):
            section = rustcs
        elif line.startswith("Code-generation features"):
            section = llvms
        elif section is not None and line.startswith("    ") and line.split():
            section.append(line.split()[0])
    cfg = lambda flag: ask([rust, "--print", "cfg", "-C", flag, *t], nightly)
    others_of = lambda printed: {line for line in printed.splitlines() if not line.startswith("target_feature=")}
    plain = others_of(ask([rust, "--print", "cfg", *t], nightly)[1])
    with concurrent.futures.ThreadPoolExecutor(os.cpu_count() or 4) as pool:
        asked = lambda flags: list(pool.map(cfg, flags))
        features, llvm, feature_cfgs = {}, {}, {}
        for name, (code, out, err) in zip(rustcs, asked([f"target-feature=+{it}" for it in rustcs])):
            if code == 0 and not err.strip():
                features[name] = sorted(set(target_features(out)) - set(base))
                # The other cfgs a feature has (wasm's atomics, `target_has_threads`).
                if others_of(out) - plain:
                    feature_cfgs[name] = sorted(others_of(out) - plain)
        only_llvm = [it for it in llvms if it not in rustcs]
        for name, (code, out, _) in zip(only_llvm, asked([f"target-feature=+{it}" for it in only_llvm])):
            added = sorted(set(target_features(out)) - set(base))
            if code == 0 and added:
                llvm[name] = added
        # What turning off a feature the target has turns off with it: those of the target's,
        # and each feature that needs it.
        off = {}
        for name in base:
            code, out, err = cfg(f"target-feature=-{name}")
            if code != 0 or err.strip():
                continue
            others = [it for it, on in features.items() if it in on]
            both = asked([f"target-feature=+{it},-{name}" for it in others])
            gone = set(base) - set(target_features(out))
            gone |= {it for it, (_, out, _) in zip(others, both) if it not in target_features(out)}
            off[name] = sorted(gone)
        sets, cpus = [], {}
        names = [line.split()[0] for line in ask([rust, "--print", "target-cpus", *t])[1].splitlines()[1:]
                 if line.split() and line.split()[0] != "native"]
        for name, (code, out, err) in zip(names, asked([f"target-cpu={it}" for it in names])):
            if code != 0 or err.strip():
                continue
            have = target_features(out)
            if have not in sets:
                sets.append(have)
            cpus[name] = sets.index(have)
    other = sorted(set(rustcs + llvms) - set(features) - set(llvm) - set(off))
    return {"features": features, "llvm_features": llvm, "features_off": off, "feature_cfgs": feature_cfgs,
            "cpus": cpus, "cpu_features": sets, "other_features": other}


def unknown_feature(rust):
    """rustc's warning of a feature the target has not, with `{feature}` for its name."""
    code, _, err = ask([rust, "--print", "cfg", "-C", f"target-feature=+{UNLISTED}"], {"RUSTC_BOOTSTRAP": "1"})
    assert code == 0 and err.endswith("warning: 1 warning emitted\n\n"), err
    return err.removesuffix("warning: 1 warning emitted\n\n").replace(UNLISTED, "{feature}")


def write():
    rust = rustc()
    verbose = said(rust, ["-vV"])
    host = next(line.split(": ", 1)[1] for line in verbose.splitlines() if line.startswith("host: "))
    old = json.load(open(DATA)) if os.path.exists(DATA) else {"targets": {}}
    data = {"version": verbose.splitlines()[0].removeprefix("rustc "),
            "verbose": verbose.replace(f"host: {host}\n", "host: {host}\n"),
            "unknown_feature": unknown_feature(rust), "targets": {}}
    if old.get("version") != data["version"]:
        old = {"targets": {}}
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    for target in TARGETS:
        t = ["--target", target]
        off = ["-C", "debug-assertions=off", "-C", "overflow-checks=off"]
        # The pinned toolchain is a nightly: `RUSTC_BOOTSTRAP=-1` makes it print a stable
        # release's cfgs, without those of unstable features.
        stable = said(rust, ["--print", "cfg", *t, *off], {"RUSTC_BOOTSTRAP": "-1"}).splitlines()
        more = said(rust, ["--print", "cfg", *t, *off], nightly).splitlines()
        panic = next(line for line in stable if line.startswith("panic="))
        files = {}
        for kind in CRATE_TYPES:
            code, out, err = ask([rust, "-", "--crate-name", "___", "--print=file-names", "--crate-type", kind, *t])
            files[kind] = out.strip() if code == 0 and not err.strip() else None
        data["targets"][target] = {
            "cfg": [line for line in stable if not line.startswith(GIVEN)],
            "nightly_cfg": [line for line in more if line not in stable and not line.startswith(GIVEN)],
            "panic": panic.split("=", 1)[1].strip('"'),
            "split_debuginfo": said(rust, ["--print", "split-debuginfo", *t]).splitlines(),
            "file_names": files,
            **features_of(rust, t, target_features("\n".join(more))),
            "target_spec_json": {
                **old["targets"].get(target, {}).get("target_spec_json", {}),
                host: said(rust, ["-Z", "unstable-options", "--print", "target-spec-json", *t], nightly),
            },
        }
    with open(DATA, "w") as out:
        json.dump(data, out, indent=1, sort_keys=True)
        out.write("\n")
    print(f"wrote what rustc {data['version']} on {host} answers for {len(TARGETS)} targets to {DATA}")



if __name__ == "__main__":
    if sys.argv[1:] != ["--write"]:
        sys.exit("usage: answers.py --write")
    write()
