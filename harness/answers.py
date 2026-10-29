#!/usr/bin/env python3
"""answers.py: blaze answers rustc's questions (`-vV`, `--print cfg`, cargo's probe of file
names, `target-spec-json` and the rest) without running rustc, byte for byte as the pinned
rustc answers them. This compares the two on every target blaze has the answers of, under the
arguments and variables cargo and rust-analyzer ask with (docs/no-rustc.md, step B).

    answers.py           compare blaze's answers with rustc's; a difference fails
    answers.py --write   write what rustc answers into blaze's data, after a toolchain bump

blaze's answers are crates/blaze-cargo/src/answers/rustc.json, of the toolchain in harness/toolchain.
Any other rustc named is answered as that one (#837): a copy of the toolchain elsewhere is asked
too, and blaze's answers to it must be the pinned toolchain's, its sysroot included. The target specs
depend on the host rustc was built for (x86-64 Linux's links with its own lld): --write keeps
those of the other hosts and writes this host's, so it is run on each host CI has (Linux
x86-64 and AArch64, macOS arm64). On a host not written (Windows) blaze answers the spec all
written hosts print, and has no answer of one they differ on.
"""
import concurrent.futures, json, os, shutil, subprocess, sys, tempfile

import norustc

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BLAZE = os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
DATA = os.path.join(ROOT, "crates/blaze-cargo/src/answers", "rustc.json")
TARGETS = [
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc", "x86_64-pc-windows-gnu",
    "wasm32-unknown-unknown", "wasm32-wasip1", "wasm32-wasip2",
]
CRATE_TYPES = ["bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"]
# What changes the cfgs rustc prints beyond the target's own; blaze adds these itself.
GIVEN = ("debug_assertions", "overflow_checks", "ub_checks", "panic=")
PROBE = ["-", "--crate-name", "___", "--print=file-names", "--crate-type", "bin", "--crate-type", "rlib",
         "--crate-type", "dylib", "--crate-type", "cdylib", "--crate-type", "staticlib",
         "--crate-type", "proc-macro", "--print=sysroot", "--print=split-debuginfo",
         "--print=crate-name", "--print=cfg", "-Wwarnings"]
# rust-analyzer's `cargo rustc --print cfg -- -O`, as cargo hands it to rustc.
LIBRARY = ["--crate-name", "word", "--edition=2021", "src/lib.rs", "--error-format=json",
           "--json=diagnostic-rendered-ansi,artifacts,future-incompat", "--crate-type", "lib",
           "--emit=dep-info,metadata,link", "-C", "embed-bitcode=no", "-C", "debuginfo=2",
           "--cfg", 'feature="default"', "--check-cfg", "cfg(docsrs,test)",
           "--check-cfg", "cfg(feature, values(\"default\"))", "-C", "metadata=d4ed28d9543870f2",
           "-C", "extra-filename=-c55bb0b8e8b3f5b4", "--out-dir", "target/debug/deps",
           "-L", "dependency=target/debug/deps", "-O", "--print", "cfg"]
# A feature no target has, of whose warning blaze keeps rustc's text.
UNLISTED = "blazeunlisted"
UI_FLAGS = ["--print", "link-args", "-C", "save-temps", "-C", "link-dead-code", "--cap-lints", "allow",
            "-L", "native=/nowhere", "--cfg", "e2024"]


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
    os.makedirs(os.path.dirname(DATA), exist_ok=True)
    with open(DATA, "w") as out:
        json.dump(data, out, indent=1, sort_keys=True)
        out.write("\n")
    print(f"wrote what rustc {data['version']} on {host} answers for {len(TARGETS)} targets to {DATA}")


def questions(work):
    """(the rustc blaze is asked as, the rustc whose answer is right, arguments, variables,
    standard input) for every question compared: of the toolchain rustup installed, and of a
    copy laid out in `work` as norustc.py lays its out, which blaze answers as the toolchain it
    builds with, not as the copy, whose sysroot it does not build with."""
    rust = rustc()
    for args, variables, stdin in about_rustc():
        yield rust, rust, args, variables, stdin
    copy = os.path.join(norustc.counted_toolchain(work, os.path.join(work, "runs.log")), "bin", "rustc")
    for args in (["--print", "sysroot"], ["--print=target-libdir"], ["-vV"], PROBE):
        yield copy, rust, args, {}, ""


def about_rustc():
    """(arguments, variables, standard input) for every question compared."""
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    yield ["-vV"], {}, ""
    yield ["-V"], {}, ""
    yield ["--version"], {}, ""
    yield ["--version", "--verbose"], {}, ""
    # servo's `[env]` has a list of its crates, which its build scripts asking rustc are given.
    crates = {"RUSTC_BOOTSTRAP": "crown,script,mozjs_sys"}
    for args in (["-vV"], ["--version"]):
        yield args, crates, ""
    for name in ("script", "libc"):
        for variables in (crates, {"RUSTC_BOOTSTRAP": "-1,script"}, {"RUSTC_BOOTSTRAP": "1,"}):
            yield ["--crate-name", name, "--print", "cfg", "-C", "debug-assertions=yes"], variables, ""
    yield PROBE, {}, ""
    yield LIBRARY, {}, ""
    yield LIBRARY, nightly, ""
    for target in [None, *TARGETS]:
        t = ["--target", target] if target else []
        yield ["--print", "sysroot", *t], {}, ""
        yield ["--print=target-libdir", *t], {}, ""
        yield ["--print", "split-debuginfo", *t], {}, ""
        yield [*PROBE[:4], *t, *PROBE[4:]], {}, ""
        yield ["-Z", "unstable-options", *t, "--print", "target-spec-json"], nightly, ""
        for variables in ({}, nightly, {"RUSTC_BOOTSTRAP": "-1"}):
            yield ["--print", "cfg", *t], variables, ""
            yield ["-O", *t, "--print", "cfg"], variables, ""
            yield ["--print", "cfg", "-C", "opt-level=3", "-C", "panic=abort", *t], variables, ""
            yield ["--print", "cfg", "-C", "opt-level=z", "-C", "debug-assertions=yes", *t], variables, ""
            yield ["--print", "cfg", "-O", "-C", "overflow-checks=on", *t], variables, ""
            yield ["--print", "cfg", "--cfg", 'feature="b"', "--cfg", "foo", "--cfg", 'feature="a"',
                   "--cfg", 'feature="b"', *t], variables, ""
        # What RUSTFLAGS of no bearing on the answers add, as count.py's `-C link-dead-code` and
        # buildmodel.py's `--print link-args -C save-temps`, which print nothing beside another.
        flags = ["-C", "link-dead-code", "-C", "force-frame-pointers=yes", "-Crpath", "-C", "force-unwind-tables=no",
                 "--print", "link-args", "-C", "save-temps", "-L", "native=/nowhere"]
        yield [*flags, "-O", *t, "--print", "cfg"], {}, ""
        yield [*PROBE[:4], *flags, *t, *PROBE[4:]], {}, ""
        # uitest.py's RUSTFLAGS, as cargo asks under them.
        yield [*UI_FLAGS, *t, "--print", "cfg"], {}, ""
        yield [*PROBE[:4], *UI_FLAGS, *t, *PROBE[4:]], {}, ""
        yield ["-", "--crate-name", "x", "--print=file-names", "--crate-type", "lib", "--crate-type", "bin",
               "--crate-type", "lib", "--print=crate-name", *t], {}, ""
    for args in features_asked(rustc()):
        yield args, nightly, ""


def features_asked(rust):
    """`--print cfg` with each CPU and each feature rustc has of each target, and some of them
    together, turned off and on; `target-cpu=native` of the host."""
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    combined = {
        "x86_64": ["-C", "target-cpu=x86-64-v3", "-C", "target-feature=+avx512f,-avx2"],
        "aarch64": ["-C", "target-cpu=neoverse-n1", "-C", "target-feature=-aes,+sve2"],
        "wasm32": ["-C", "target-feature=+simd128,+relaxed-simd", "-C", "target-feature=-simd128"],
    }
    # Of another architecture: rustc warns of each and turns nothing on (buildmodel's
    # rustflags_target_feature on an AArch64 host).
    unlisted = {"x86_64": "+neon,-sve2", "aarch64": "+avx2,+fma", "wasm32": "+avx2"}
    yield ["--print", "cfg", "-C", "target-cpu=native"]
    yield ["--print", "cfg", "-C", "target-cpu=native", "-C", "target-feature=-neon,+avx2"]
    for target in TARGETS:
        t = ["--target", target]
        for name in ask([rust, "--print", "target-cpus", *t])[1].splitlines()[1:]:
            if name.split():
                yield ["--print", "cfg", "-C", f"target-cpu={name.split()[0]}", *t]
        listed = ask([rust, "--print", "target-features", *t], nightly)[1].split("Code-generation features")[0]
        names = [line.split()[0] for line in listed.splitlines() if line.startswith("    ") and line.split()]
        for name in names:
            yield ["--print", "cfg", "-C", f"target-feature=+{name}", *t]
        for name in names[::7]:
            yield ["--print", "cfg", "-C", f"target-feature=-{name}", *t]
            yield ["--print", "cfg", "-C", "opt-level=3", "-C", f"target-feature=+{name},-{names[0]}", *t]
        yield ["--print", "cfg", *combined[target.split("-")[0]], *t]
        arch = target.split("-")[0]
        yield ["--print", "cfg", "-C", f"target-feature={unlisted[arch]}", *t]
        yield ["--print", "cfg", "-C", f"target-feature={unlisted[arch]}", "-A", "warnings", *t]
        yield ["--print", "cfg", "-C", "opt-level=3", *combined[arch], "-C", f"target-feature={unlisted[arch]}", *t]


def must_answer(warned):
    """Whether what rustc warns of is only a kind of crate the target does not have, which
    cargo's probe of every target meets, or features it has not: blaze answers those as rustc does."""
    lines = [line for line in warned.splitlines() if line.strip()]
    return all(line.startswith(("warning: dropping unsupported crate type",
                                "warning: unknown and unstable feature specified for `-Ctarget-feature`",
                                "  |", "  = ")) or
               (line.startswith("warning: ") and line.endswith((" warning emitted", " warnings emitted")))
               for line in lines)


def check():
    rust = rustc()
    if not os.path.exists(DATA):
        print(f"blaze has no answers of rustc {TOOLCHAIN}: answers.py --write writes them")
        sys.exit(1)
    data = json.load(open(DATA))
    host = next(line.split(": ", 1)[1] for line in said(rust, ["-vV"]).splitlines() if line.startswith("host: "))
    home = tempfile.mkdtemp(prefix="blaze-answers-")
    wrong, answered, unknown = [], 0, {}

    def compared(question):
        rust, truth, args, variables, stdin = question
        want = ask([truth, *args], variables, stdin)
        if want[0] != 0:
            return None
        if "target-spec-json" in args:
            target = args[args.index("--target") + 1] if "--target" in args else host
            specs = data["targets"].get(target, {}).get("target_spec_json", {})
            # Of a host not recorded, blaze answers the spec every recorded host prints.
            if host not in specs and len(set(specs.values())) != 1:
                # rustc is asked, once per toolchain.
                return "unknown", target, want[1]
        got = ask([BLAZE, "rustc-wrapper", rust, *args],
                  {**variables, "BLAZE_HOME": home}, stdin)
        if want[2].strip() and not must_answer(want[2]) and got[0] != 0:
            # A warning blaze does not know it refuses rather than leaves out.
            return None
        return "asked", (args, variables, want[1:], got), got == want

    work = tempfile.mkdtemp(prefix="blaze-answers-", dir="/tmp" if sys.platform == "darwin" else None)
    with concurrent.futures.ThreadPoolExecutor(os.cpu_count() or 4) as pool:
        results = list(pool.map(compared, questions(work)))
    shutil.rmtree(work, ignore_errors=True)
    for result in results:
        if result is None:
            continue
        if result[0] == "unknown":
            unknown[result[1]] = result[2]
            continue
        answered += 1
        if not result[2]:
            wrong.append(result[1])
    for args, variables, want, (code, out, err) in wrong:
        print(f"DIFFERS: {' '.join(args)} {variables or ''}: exit {code}\n  rustc:  {want[0]!r} {want[1]!r}\n"
              f"  blaze: {out!r} {err!r}")
    print(f"{answered - len(wrong)} of {answered} questions answered by blaze as rustc {TOOLCHAIN} answers them")
    if unknown:
        print(f"blaze has not the target specs of rustc on {host}, which rustc is asked for; "
              f"answers.py --write on this host adds them:\n{json.dumps(unknown)}")
    if wrong:
        sys.exit(1)


if __name__ == "__main__":
    write() if "--write" in sys.argv else check()
