#!/usr/bin/env python3
"""bunprep.py: bun's Rust port (oven-sh/bun from v1.4) as a workspace whose executable rustc links.

    bunprep.py CHECKOUT    prepares CHECKOUT-native (below) and prints the environment

Bun's own build (`bun scripts/build.ts`) compiles its C and C++ with ninja (JavaScriptCore from a
prebuilt WebKit, BoringSSL, uSockets, ...), builds its Rust as one static library
(`bun_runtime`) and links the two with clang. blaze builds and splices executables that rustc
links, so the tree measured has one more workspace member, `bun-exe`: a `#![no_main]` binary of
`bun_runtime`, whose `main` it is, with the native objects and libraries on rustc's link line.

`prepare(checkout)` copies the clone (which nothing writes to) to `CHECKOUT-native` once per
revision and configures bun's release build there without LTO. Ninja then builds what bun's link
step (in build.ninja) takes besides bun's Rust, the code that Rust includes and the dependencies
it has under `vendor/`, but not the Rust: that is what is measured. The native half of the link
step goes to a file of link arguments that `bun-exe`'s build script reads (BUN_NATIVE_LINK_ARGS),
and the member is added. It returns that tree, to be copied without its `build` directory, and
the environment every build of it needs: bun's pinned toolchain, the code bun's build generated
(BUN_CODEGEN_DIR) and the link arguments. `in_place(ws)` does the same in a checkout of its own
(appruns.py's `prepare`). `sources(ws)` only fetches the crates bun's workspace has under
`vendor/`, what `cargo metadata` needs.

What bun's build needs is on PATH: what docs/project/contributing.mdx lists (LLVM 23, cmake,
ninja, go, a bun to run its scripts), as `nix develop .#bench-bun` has it, whose only Rust is
bun's pinned nightly. With rustup, that nightly is installed when it is missing.
docs/research/bun.md says what was measured with it.
"""
import hashlib, os, re, shlex, shutil, subprocess, sys, tarfile, urllib.request

# The checkout's rust-toolchain.toml.
TOOLCHAIN = "nightly-2026-09-15"
PROFILE = "release"
# Holds the revision the tree was built at and this file's hash: a tree prepared by an older
# bunprep is built again, from its own build.
MARKER = ".bunprep-built"
SELF = hashlib.sha256(open(__file__, "rb").read()).hexdigest()[:16]
FRAME_POINTERS = "[target.'cfg(all())']\nrustflags = [\"-C\", \"force-frame-pointers=yes\"]\n"

# Flags of bun's link that are not kept, by why. Every other flag is: `--wrap`, the exported
# symbols, the stack size and the libraries are what the program is.
DROPPED = [
    # An executable linked without `.eh_frame_hdr` has no index of its frames: blaze links
    # such a program again at every edit instead of adding the change to it. One linked
    # without compact unwind info is spliced (#33); bun's numbers are of the link without it.
    r"-Wl,-no_compact_unwind", r"-Wl,--no-eh-frame-hdr",
    # Files bun's own link writes or reads: its map, its LTO object, its symbol order.
    r"-Wl,-map,.*", r"-Wl,-Map=.*", r"-Wl,-object_path_lto,.*", r"-Wl,-order_file,.*",
    r"-Wl,--symbol-ordering-file=.*", r"-Wl,--no-warn-symbol-ordering",
    # What rustc says itself: the deployment target and SDK, dead code. Not that a Linux
    # executable is not position independent (`-fno-pic -Wl,-no-pie`): bun's objects and the
    # prebuilt WebKit are compiled for that, and lld cannot put them in a PIE.
    r"-mmacosx-version-min=.*", r"-dead_strip", r"-dead_strip_dylibs",
    # LTO, which a dev build and `--lto=off` have none of; the linker bun's link chose.
    r"-flto.*", r"-Wl,--lto-.*", r"-Wl,-mllvm,.*", r"-Wl,-ld_new", r"-fuse-ld=.*",
    # A flag of the compiler, not of the link.
    r"-fno-keep-static-consts",
]
# Flags whose value is the next argument.
WITH_VALUE = {"-isysroot", "-exported_symbols_list", "-Xlinker", "-arch", "-target"}
DROPPED_WITH_VALUE = {"-isysroot"}

MAIN = """#![no_main]
// `bun_runtime` defines `main`; rustc links it with the native objects (build.rs).
extern crate bun_runtime;
"""
BUILD_RS = """fn main() {
    println!("cargo:rerun-if-env-changed=BUN_NATIVE_LINK_ARGS");
    let args = std::env::var("BUN_NATIVE_LINK_ARGS").expect("BUN_NATIVE_LINK_ARGS names the file of native link arguments");
    println!("cargo:rerun-if-changed={args}");
    for line in std::fs::read_to_string(&args).unwrap().lines() {
        if !line.is_empty() {
            println!("cargo:rustc-link-arg-bins={line}");
        }
    }
}
"""
SANDBOX_ENV = '\n[workspace.metadata.blaze.sandbox]\nbuild-script-env = ["BUN_CODEGEN_DIR", "BUN_NATIVE_LINK_ARGS"]\n'
MANIFEST = """[package]
name = "bun-exe"
version = "0.0.0"
edition = "2024"
publish = false

[[bin]]
name = "bun"
path = "main.rs"

[dependencies]
bun_runtime = { path = "../src/runtime", features = ["rustc-links"] }

[lints]
workspace = true
"""


def rust_env(env):
    """`env` with bun's toolchain and none of the shell's flags for cargo."""
    env = dict(env, RUSTUP_TOOLCHAIN=TOOLCHAIN)
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "RUSTC_WRAPPER", "CARGO_BUILD_TARGET", "RUSTC"):
        env.pop(name, None)
    return env


def install_toolchain():
    rustup = shutil.which("rustup")
    if rustup and TOOLCHAIN not in subprocess.run([rustup, "toolchain", "list"], capture_output=True, text=True).stdout:
        subprocess.run([rustup, "toolchain", "install", TOOLCHAIN, "--profile", "minimal"], check=True)


def build_dir(tree):
    return os.path.join(tree, "build", PROFILE)


def link_statement(ninja):
    """The inputs and `ldflags` of the statement that links bun-profile in build.ninja, and the
    objects it links lazily: on Linux every dependency's, in a response file that `lazy` names,
    between `--start-lib` and `--end-lib`."""
    lines = open(ninja).read().replace("$\n", "").split("\n")
    at = next(i for i, line in enumerate(lines) if re.match(r"build bun-profile[ |:]", line))
    _, rest = lines[at].split(": ", 1)
    inputs = []
    for token in rest.split()[1:]:
        if token in ("|", "||"):
            break
        inputs.append(token)
    flags, lazy = "", []
    for line in lines[at + 1:]:
        if not line.startswith(" "):
            break
        if line.strip().startswith("ldflags = "):
            flags = line.strip().removeprefix("ldflags = ")
        if line.strip().startswith("lazy = "):
            for flag in shlex.split(line.strip().removeprefix("lazy = ")):
                rsp = flag.removeprefix("-Wl,@")
                assert rsp != flag, f"bun-profile's lazy objects are not a response file: {flag}"
                lazy += open(os.path.join(os.path.dirname(ninja), rsp)).read().split()
    return inputs, shlex.split(flags), lazy


def native_targets(tree):
    """What ninja builds for the measured builds: the native inputs of bun's link, the code its
    Rust includes (`rust-codegen-ready`) and the dependencies its Cargo.toml has under `vendor/`
    (`clone-<name>`). Not bun's own build of its Rust: that is what is measured."""
    inputs, _, lazy = link_statement(os.path.join(build_dir(tree), "build.ninja"))
    cloned = re.findall(r'path = "vendor/([^"/]+)"', open(os.path.join(tree, "Cargo.toml")).read())
    return [*(path for path in inputs if not path.startswith("rust-target/")),
            *(path for path in lazy if not path.startswith("-")), "rust-codegen-ready",
            *(f"clone-{name}" for name in cloned)]


def link_args(tree):
    """The native half of bun's link, one argument per line in build/release/blaze-link.args:
    every object and library but bun's own Rust (which `bun-exe` is), and the flags kept."""
    out = build_dir(tree)
    inputs, flags, lazy = link_statement(os.path.join(out, "build.ninja"))
    native = [os.path.join(out, path) for path in inputs if not path.startswith("rust-target/")]
    objects = [path for path in native if path.endswith(".o")]
    filelist = os.path.join(out, "blaze-link.objects")
    open(filelist, "w").write("\n".join(objects) + "\n")
    darwin = sys.platform == "darwin"
    args = [f"-Wl,-filelist,{filelist}"] if darwin else [f"@{filelist}"]
    if lazy:
        lazylist = os.path.join(out, "blaze-link.lazy")
        open(lazylist, "w").write("\n".join(it if it.startswith("-") else os.path.join(out, it) for it in lazy) + "\n")
        args.append(f"-Wl,@{lazylist}")
    args += [path for path in native if not path.endswith(".o")]
    kept = iter(flags)
    for flag in kept:
        if flag in WITH_VALUE:
            value = next(kept)
            if flag not in DROPPED_WITH_VALUE:
                args += [flag, value]
        elif not any(re.fullmatch(pattern, flag) for pattern in DROPPED):
            args.append(flag)
    # rustc links with -nodefaultlibs, so the C++ library clang++ would add is named: on Linux
    # the archive, as bun's `-static-libstdc++` has it, which no loader has to find.
    args.append("-lc++" if darwin else "-l:libstdc++.a" if "-static-libstdc++" in flags else "-lstdc++")
    system = system_parts(tree, os.environ)
    if system:
        args.append(f"-L{system}/lib")
    path = os.path.join(out, "blaze-link.args")
    open(path, "w").write("\n".join(args) + "\n")
    return path


# What bun takes of macOS itself rather than building (ICU, `libicucore`, and SQLite), which
# nixpkgs' SDK leaves out of the headers and stubs it ships.
SYSTEM = ["include/unicode", "include/sqlite3.h", "include/sqlite3ext.h", "lib/libicucore.tbd", "lib/libsqlite3.tbd"]


def system_parts(tree, env):
    """On macOS, a directory with what of `SYSTEM` Apple's SDK has and the SDK the compiler is
    given does not: `include` for the compiler, `lib` for the link. `None` where nothing is
    missing."""
    sdk = env.get("SDKROOT")
    if sys.platform != "darwin" or (sdk and os.path.exists(os.path.join(sdk, "usr/include/unicode/utypes.h"))):
        return None
    plain = {k: v for k, v in env.items() if k not in ("SDKROOT", "DEVELOPER_DIR")}
    apple = subprocess.run(["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"], env=plain,
                           capture_output=True, text=True, check=True).stdout.strip()
    system = os.path.join(build_dir(tree), "blaze-system")
    for path in SYSTEM:
        target = os.path.join(apple, "usr", path)
        if not os.path.exists(target):
            raise SystemExit(f"bunprep: bun's build on macOS needs what the system has of it, and {target} is missing")
        link = os.path.join(system, path)
        os.makedirs(os.path.dirname(link), exist_ok=True)
        if os.path.lexists(link):
            os.remove(link)
        os.symlink(target, link)
    return system


def overlay(tree):
    """Adds the member `bun-exe` and the switch it needs in bun_runtime. Each step is skipped
    when it is there already."""
    os.makedirs(os.path.join(tree, "bun-exe"), exist_ok=True)
    for name, text in (("Cargo.toml", MANIFEST), ("main.rs", MAIN), ("build.rs", BUILD_RS)):
        open(os.path.join(tree, "bun-exe", name), "w").write(text)
    edit(os.path.join(tree, "Cargo.toml"), '  "src/bunfig",\n', '  "src/bunfig",\n  "bun-exe",\n')
    # A sandboxed build script is given only what cargo documents, what it says it reads and
    # what the root manifest names (docs/cargo.md): bun_core's reads BUN_CODEGEN_DIR without
    # saying so, and bun-exe's needs BUN_NATIVE_LINK_ARGS on its first run.
    manifest = os.path.join(tree, "Cargo.toml")
    if SANDBOX_ENV not in open(manifest).read():
        open(manifest, "a").write(SANDBOX_ENV)
    # Bun's C and C++ are its release build, without ASSERT_ENABLED; with debug assertions the
    # Rust calls the functions that only a build with them has.
    edit(os.path.join(tree, "Cargo.toml"), '[profile.dev]\npanic = "abort"\n',
         '[profile.dev]\npanic = "abort"\ndebug-assertions = false\n')
    # bun_runtime writes the allocator shim's marker for bun's own link of the static library;
    # rustc writes it when it links the executable.
    edit(os.path.join(tree, "src/runtime/Cargo.toml"), "[features]\ndefault = []\n",
         "[features]\ndefault = []\nrustc-links = []\n")
    edit(os.path.join(tree, "src/runtime/bin_entry/mod.rs"), "\n#[rustc_std_internal_symbol]\nfn __rust_no_alloc_shim_is_unstable_v2",
         '\n#[cfg(not(feature = "rustc-links"))]\n#[rustc_std_internal_symbol]\nfn __rust_no_alloc_shim_is_unstable_v2')
    if sys.platform == "darwin":
        # Bun's configure has clang++ drive ld64.lld; the system's ld64 links the executable
        # here, as for every other project the harness builds on macOS.
        cxx = re.search(r'\[target\.aarch64-apple-darwin\][^\n]*\nlinker = "([^"]+)"', open(os.path.join(tree, ".cargo/config.toml")).read())
        open(os.path.join(tree, ".cargo/config.toml"), "w").write(
            f'[target.aarch64-apple-darwin]\nlinker = "{cxx[1] if cxx else "clang++"}"\n')
    # Bun's own build keeps frame pointers (scripts/build/rust.ts), and `bun_core::return_address`
    # reads its caller's frame by them: on x86-64, where the register is otherwise one like the
    # rest, a build without them faults there on the first `-e`. Joined with the target's own.
    config = os.path.join(tree, ".cargo/config.toml")
    if FRAME_POINTERS not in open(config).read():
        open(config, "a").write("\n" + FRAME_POINTERS)


def edit(path, old, new):
    text = open(path).read()
    if new in text:
        return
    assert old in text, f"{old!r} is not in {path}"
    open(path, "w").write(text.replace(old, new, 1))


def env_of(tree):
    out = build_dir(tree)
    return {"RUSTUP_TOOLCHAIN": TOOLCHAIN,
            "BUN_CODEGEN_DIR": os.path.join(out, "codegen"), "BUN_NATIVE_LINK_ARGS": os.path.join(out, "blaze-link.args")}


def head_of(tree):
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=tree, capture_output=True, text=True,
                          check=True).stdout.strip()


def native(tree, head):
    """Bun's own build of the tree, its link and the member `bun-exe`, once per revision."""
    marker = os.path.join(tree, MARKER)
    if os.path.exists(marker) and open(marker).read().split() == [head, SELF]:
        return
    env = rust_env(os.environ)
    if sys.platform == "darwin" and not env.get("SDKROOT"):
        env["SDKROOT"] = subprocess.run(["xcrun", "--show-sdk-path"], capture_output=True, text=True, check=True).stdout.strip()
    system = system_parts(tree, env)
    if system:
        env["NIX_CFLAGS_COMPILE"] = f"{env.get('NIX_CFLAGS_COMPILE', '')} -isystem {system}/include".strip()
    bun = env.get("BUN_BOOTSTRAP") or shutil.which("bun", path=env["PATH"]) or "bun"
    print(f"bunprep: bun's own build of {head[:10]}'s C and C++ in {tree} (once per revision)", flush=True)
    log = open(os.path.join(tree, "bunprep-build.log"), "w")

    def run(argv):
        log.write(f"$ {shlex.join(argv)}\n")
        log.flush()
        if subprocess.run(argv, cwd=tree, env=env, stdout=log, stderr=subprocess.STDOUT).returncode != 0:
            log.close()
            raise SystemExit(f"bun's own build failed (the log is {tree}/bunprep-build.log):\n"
                             + open(log.name).read()[-3000:])

    # A release build without LTO compresses its objects' debug sections with zstd, which
    # nixpkgs' LLVM is built without ("cannot compress debug sections (zstd not enabled)");
    # bun's ELF link compresses them with zlib, and so do its objects here. Mach-O has no
    # compressed sections, and the objects there are left as they are.
    edit(os.path.join(tree, "scripts/build/flags.ts"), 'flag: ["-g", "-gz=zstd"],',
         'flag: ["-g"],' if sys.platform == "darwin" else 'flag: ["-g", "-gz=zlib"],')
    run([bun, "scripts/build.ts", f"--profile={PROFILE}", "--lto=off", "--configure-only"])
    run(["ninja", "-C", build_dir(tree), *native_targets(tree)])
    log.close()
    link_args(tree)
    overlay(tree)
    # The lock file gets the new member, so that `--locked` builds take it.
    subprocess.run(["cargo", "metadata", "--format-version", "1"], cwd=tree, env=env, check=True,
                   stdout=subprocess.DEVNULL)
    open(marker, "w").write(f"{head} {SELF}\n")


def prepare(checkout):
    """A copy of the checkout with bun's own build in it, and the environment its builds need
    (see the module's text). The checkout itself is not changed."""
    install_toolchain()
    tree = checkout.rstrip("/") + "-native"
    head = head_of(checkout)
    marker = os.path.join(tree, MARKER)
    if not (os.path.exists(marker) and open(marker).read().split()[:1] == [head]):
        shutil.rmtree(tree, ignore_errors=True)
        shutil.copytree(checkout, tree, symlinks=True)
    native(tree, head)
    return tree, env_of(tree)


def sources(ws):
    """What `cargo metadata` needs of a checkout without bun's own build: the crates its
    Cargo.toml has under `vendor/`, which that build fetches (scripts/build/deps/<name>.ts) at
    the commit the file pins, with the `.patch` files it lists applied as that build applies them
    (rust-argon2's lifts its memory floor). Enough for what reads the workspace without building
    it."""
    for name in re.findall(r'path = "vendor/([^"/]+)"', open(os.path.join(ws, "Cargo.toml")).read()):
        dep = open(os.path.join(ws, "scripts", "build", "deps", f"{name}.ts")).read()
        if re.search(r'kind: "([^"]+)"', dep)[1] != "github-archive":
            raise SystemExit(f"bunprep: vendor/{name} is not fetched from a GitHub archive")
        repo = re.search(r'repo: "([^"]+)"', dep)[1]
        commit = re.search(r'commit: (\w+)', dep)[1]
        commit = re.search(rf'const {commit} = "([0-9a-f]{{40}})"', dep)[1]
        listed = re.search(r"patches: \[([^\]]*)\]", dep)
        patches = re.findall(r'"([^"]+)"', listed[1]) if listed else []
        if "patches:" in dep and not listed or any(not it.endswith(".patch") for it in patches):
            raise SystemExit(f"bunprep: vendor/{name} has patches other than a list of `.patch` files")
        # The commit and the patches' contents, as bun's build identifies a fetched source.
        identity = hashlib.sha256("\0".join([commit, *(open(os.path.join(ws, it)).read() for it in patches)])
                                  .encode()).hexdigest()
        into = os.path.join(ws, "vendor", name)
        stamp = os.path.join(into, ".bunprep-commit")
        if os.path.exists(stamp) and open(stamp).read().strip() == identity:
            continue
        shutil.rmtree(into, ignore_errors=True)
        os.makedirs(into)
        url = f"https://github.com/{repo}/archive/{commit}.tar.gz"
        with urllib.request.urlopen(url) as archive, tarfile.open(fileobj=archive, mode="r|gz") as tar:
            for member in tar:
                member.name = member.name.partition("/")[2]
                if member.name:
                    tar.extract(member, into, filter="data")
        # From the checkout's root: in a directory of a repository, `git apply` takes the
        # patch's paths as the repository's.
        for patch in patches:
            subprocess.run(["git", "apply", f"--directory=vendor/{name}", patch], cwd=ws, check=True)
        open(stamp, "w").write(f"{identity}\n")


def in_place(ws):
    """`prepare` in the checkout itself: what `apps.json`'s `"prepare": "bunprep"` runs."""
    install_toolchain()
    native(ws, head_of(ws))
    return env_of(ws)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    tree, env = prepare(os.path.abspath(sys.argv[1]))
    print(tree)
    for name, value in env.items():
        print(f"export {name}={shlex.quote(value)}")
