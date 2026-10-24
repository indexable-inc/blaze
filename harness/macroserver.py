#!/usr/bin/env python3
"""macroserver.py: the release's proc-macro server, built by ra-jit, expanding with the
libraries ra-jit links, held against the toolchain's server with rustc's (docs/no-rustc.md,
step J1).

ra-jit builds the server of `vendor/proc-macro-srv` (`--exe`), and each proc-macro crate a
package of serde, thiserror, tokio and clap depends on as a shared library with the list of
its macros in it (`--exe` with `--shared`, step J2). It then builds the package with
`RA_JIT_PROC_MACRO_COMPARE` naming that server (crates/ra-jit-expand/src/compare.rs): each
call the toolchain's server expands with rustc's library, derives with helper attributes,
attributes and function-like macros, is expanded again by ra-jit's server with ra-jit's
library, and every output has to be the toolchain's, spans and all.

Then the package is built whole by ra-jit into its own target directory (`--own-target`) and
run, twice: once as before, where the load's `cargo check --compile-time-deps` has rustc build
the proc macros, and once with `RA_JIT_PROC_MACRO_SERVER` naming the server, where ra-jit links
each of them (crates/ra-jit/src/cargo/own_macro.rs) and that server expands with them. Each
program has to print what cargo's does, and the proc-macro libraries in the target directory
have to be all rustc's, then all ra-jit's.

With the names of bench apps (nushell, helix) it measures those instead: each is built whole
by ra-jit into its own target directory, without the server named and with it, and timed.

    python3 macroserver.py
    python3 macroserver.py nushell helix
"""
import glob, json, os, platform, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from allrajit import std_of
from difftest import RA_JIT
from storelib import scratch

SERVER = os.path.join(HERE, "..", "vendor", "proc-macro-srv")
SERVER_PACKAGE = "proc-macro-srv-cli"
SERVER_BIN = "rust-analyzer-proc-macro-srv"

MANIFEST = """[package]
name = "macrouse"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1", features = ["macros", "rt"] }
clap = { version = "4", features = ["derive"] }

[workspace]
"""
SOURCE = r"""use std::collections::BTreeMap;

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Point {
    x_pos: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(rename = "kind")]
    shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Shape {
    Circle { r: f64 },
    Square { side: u32 },
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize)]
pub struct Wrapper<'a, T: Clone> {
    #[serde(borrow)]
    name: &'a str,
    items: Vec<T>,
    #[serde(flatten)]
    extra: BTreeMap<String, u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad value {value} at {line}")]
    Bad { value: i64, line: usize },
    #[error(transparent)]
    Format(#[from] std::fmt::Error),
}

#[derive(Debug, thiserror::Error)]
#[error("wrapped: {source}")]
pub struct Wrapped {
    #[source]
    source: AppError,
}

/// A fixture's command line.
#[derive(Parser, Debug)]
#[command(name = "macrouse", version, about = "a fixture")]
pub struct Cli {
    /// How many.
    #[arg(short, long, default_value_t = 3)]
    count: u32,
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[command(flatten)]
    shared: Shared,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Runs it.
    Run {
        #[arg(short)]
        fast: bool,
    },
    Stop,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum Mode {
    Quick,
    Slow,
}

#[derive(clap::Args, Debug)]
pub struct Shared {
    #[arg(long)]
    verbose: bool,
}

async fn twice(n: u32) -> u32 {
    n * 2
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse_from(["macrouse", "--count", "2", "run", "-f"]);
    let doubled = tokio::select! {
        biased;
        n = twice(cli.count) => n,
        _ = tokio::task::yield_now() => 0,
    };
    let err = AppError::Bad { value: -1, line: doubled as usize };
    println!("{cli:?} {doubled} {err}");
    println!("{}", Wrapped { source: err });
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn doubles() {
        assert_eq!(super::twice(2).await, 4);
    }
}
"""
# The proc-macro crates the package's build expands with, by package.
MACRO_CRATES = {"serde_derive", "thiserror-impl", "tokio-macros", "clap_derive"}
# The name of the static ra-jit lists a proc-macro library's macros in.
LIST = b"RA_JIT_PROC_MACROS"
# The bench apps it measures: the crate ra-jit is asked for and the program it links.
APPS = {"nushell": ("nu", "nu"), "helix": ("hx", "hx")}


def run(cmd, cwd, what, env=None):
    start = time.monotonic()
    ran = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0", **(env or {})})
    took = time.monotonic() - start
    if ran.returncode != 0:
        raise SystemExit(f"FAIL: {what} exited {ran.returncode}:\n{ran.stdout[-4000:]}"
                         f"{ran.stderr[-8000:]}")
    return ran, took


def platform_triple():
    return f"{platform.machine()}-unknown-linux-gnu"


def metadata(root):
    ran, _ = run(["cargo", "metadata", "--format-version", "1", "--filter-platform",
                  platform_triple()], root, f"cargo metadata of {root}")
    return json.loads(ran.stdout)


def lib_name(package):
    for target in package["targets"]:
        if {"lib", "rlib", "proc-macro"} & set(target["kind"]):
            return target["name"].replace("-", "_")
    return None


def closure(meta, name):
    """The package called `name` and the libraries it links: its normal dependencies, all the
    way down, but proc-macro crates, which run while it is built. Each by its crate's name."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    start = next(p["id"] for p in meta["packages"] if p["name"] == name)
    seen, todo, crates = {start}, [start], []
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            if not any(kind["kind"] is None for kind in dep["dep_kinds"]):
                continue
            package = packages[dep["pkg"]]
            if any("proc-macro" in t["kind"] for t in package["targets"]):
                continue
            if dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                todo.append(dep["pkg"])
                crates.append(lib_name(package))
    return crates


def std_source():
    """The toolchain's `library` directory, whose crates rust-analyzer loads as the standard
    library's."""
    toolchain = open(os.path.join(HERE, "toolchain")).read().strip()
    home = os.environ.get("RUSTUP_HOME", os.path.expanduser("~/.rustup"))
    found = glob.glob(os.path.join(home, "toolchains", f"{toolchain}-*", "lib", "rustlib",
                                   "src", "rust", "library"))
    if not found:
        raise SystemExit(f"FAIL: no source of the standard library of {toolchain} in {home}")
    return os.path.realpath(found[0])


def with_std(crates, panic="unwind"):
    """`crates`, then the standard library's and `proc_macro`. A crate of the library that has the name of one
    of `crates` (libc, hashbrown, memchr, cfg_if) is named by its root file, as a name is the
    program's crate's."""
    vendored = {}
    for root in glob.glob(os.path.join(std_source(), "vendor", "*", "src", "lib.rs")):
        package = os.path.basename(os.path.dirname(os.path.dirname(root)))
        vendored[package.rsplit("-", 1)[0].replace("-", "_")] = root
    named = set(crates)
    # `proc_macro` and what it depends on outside the library's unit.
    library = [*std_of(panic), "proc_macro", "rustc_literal_escaper"]
    std = [vendored[c] if c in named and c in vendored else c for c in library]
    return [*crates, *std]


def ra_jit(root, crates, out, *extra, env=None):
    args = sum((["--crate", c] for c in crates), [])
    return run([RA_JIT, root, *args, "--out", out, *extra], root, f"ra-jit {crates[0]}", env)


def linked(ran, what):
    said = re.search(r"^linked .* in (\S+), (\d+) functions in it ra-jit could not compile$",
                     ran.stderr, re.M)
    if not said:
        raise SystemExit(f"FAIL: {what}: ra-jit did not say it linked:\n{ran.stderr[-8000:]}")
    if said.group(2) != "0":
        raise SystemExit(f"FAIL: {what}: {said.group(2)} functions ra-jit could not compile:\n"
                         f"{ran.stderr[-8000:]}")


def built_macros(target):
    """The proc-macro libraries the load's cargo built in `target`, ra-jit's ones apart."""
    libraries = glob.glob(os.path.join(target, "**", "deps", "lib*.so"), recursive=True)
    ours = {lib for lib in libraries if LIST in open(lib, "rb").read()}
    name = lambda lib: os.path.basename(lib)[3:].rsplit("-", 1)[0]
    return sorted(map(name, ours)), sorted(map(name, set(libraries) - ours))


def wired(work, root, meta, server):
    """The package built whole by ra-jit and run, its proc macros built by the load's cargo in
    ra-jit's own target directory (`--own-target`): by rustc, and then by ra-jit with the
    server named, which expands with its libraries."""
    program = os.path.join(work, "macrouse-ra-jit")
    cargo_target = os.path.join(work, "cargo-target")
    ran, _ = run(["cargo", "run", "-q"], root, "cargo run", env={"CARGO_TARGET_DIR": cargo_target})
    expected = ran.stdout
    crates = with_std(["macrouse", *closure(meta, "macrouse")])
    lines, wanted = [], sorted(lib_name(p) for p in meta["packages"] if p["name"] in MACRO_CRATES)
    for named in (False, True):
        # A target directory of its own is seeded from cargo's, whose proc macros are rustc's.
        shutil.rmtree(os.path.join(root, "target"), ignore_errors=True)
        env = {"RA_JIT_PROC_MACRO_SERVER": server} if named else {}
        ran, took = ra_jit(root, crates, os.path.join(work, "wired.o"), "--own-target",
                           "--exe", program, env=env)
        how = "with the server named" if named else "without it"
        linked(ran, f"the package ({how})")
        got = run([program], root, f"the package ({how})")[0].stdout
        if got != expected:
            raise SystemExit(f"FAIL: the package ({how}) printed otherwise than cargo's:\n"
                             f"cargo:  {expected}\nra-jit: {got}")
        ours, theirs = built_macros(os.path.join(root, "target", "rust-analyzer"))
        if (ours, theirs) != ((wanted, []) if named else ([], wanted)):
            raise SystemExit(f"FAIL: the load ({how}) built the proc macros {ours} by ra-jit and "
                             f"{theirs} by rustc, not each of {wanted} by "
                             f"{'ra-jit' if named else 'rustc'}:\n{ran.stderr[-8000:]}")
        lines.append(f"the package whole, its proc macros by {'ra-jit' if named else 'rustc'}: "
                     f"built and linked in {took:.1f} s, prints what cargo's does")
    return lines


def app(name, work, server):
    """A bench app of bench.py built whole by ra-jit into its own target directory, from
    nothing on a store it filled before, twice without the server named and twice with it:
    how long each took, what the load's cargo built by rustc, and what the program prints."""
    import bench
    spec, (crate, program) = bench.PROJECTS[name], APPS[name]
    root = os.path.join(work, name)
    run(["git", "clone", "-q", "--filter=blob:none", f"https://github.com/{spec['repo']}", root],
        work, f"git clone of {name}")
    run(["git", "-C", root, "checkout", "-q", "--detach", spec["rev"]], work, f"checkout of {name}")
    binary = os.path.join(work, program)
    lines = []
    for named in (False, False, True, True):
        shutil.rmtree(os.path.join(root, "target"), ignore_errors=True)
        env = {**spec.get("env", {}), "RA_JIT_CACHE": os.path.join(work, "store")}
        if named:
            env["RA_JIT_PROC_MACRO_SERVER"] = server
        ran, took = ra_jit(root, [crate], os.path.join(work, f"{name}.o"), "--own-target",
                           "--exe", binary, env=env)
        left = re.search(r"(\d+) functions in it ra-jit could not compile$", ran.stderr, re.M)
        own = os.path.join(root, "target", "rust-analyzer")
        ours, theirs = built_macros(own)
        rlibs = glob.glob(os.path.join(own, "**", "deps", "lib*.rlib"), recursive=True)
        scripts = glob.glob(os.path.join(own, "**", "build", "*", "build-script-build"),
                            recursive=True)
        said = subprocess.run([binary, *spec["run"]], cwd=root, capture_output=True, text=True,
                              timeout=60, env={**os.environ, **spec.get("env", {})})
        load = re.search(r"^load workspace: (\S+)$", ran.stderr, re.M)
        start = len(lines)
        lines.append(f"{name}, {'with' if named else 'without'} the server: {took:.1f} s, the "
                     f"load {load.group(1) if load else '?'}; the load's cargo built "
                     f"{len(theirs)} proc macros by rustc and {len(ours)} by ra-jit, "
                     f"{len(rlibs)} libraries and {len(scripts)} build scripts; "
                     f"{left.group(1) if left else '?'} functions left; `{program} "
                     f"{' '.join(spec['run'])}` exited {said.returncode}: "
                     f"{said.stdout.strip()[:80]!r}")
        lines += [f"  left: {it[:300]}" for it in re.findall(r"^  left to rustc: (.*)$", ran.stderr, re.M)
                  if "no rustc symbol" not in it and "calling convention Custom" not in it]
        if named:
            lines += [f"  {it}" for it in fallbacks(own, theirs)]
        print("\n".join(lines[start:]), flush=True)
    return lines


def fallbacks(own, theirs):
    """Why each proc macro of `theirs` is rustc's: the last line ra-jit's build of it said."""
    said = []
    for name in theirs:
        logs = glob.glob(os.path.join(own, "**", ".ra-jit-units", f"{name}-*", "*", "ra-jit.log"),
                         recursive=True)
        last = [l for log in logs for l in open(log).read().splitlines() if l.strip()][-1:]
        said.append(f"{name}: {last[0][:300] if last else 'ra-jit did not build it'}")
    return said


def built_server(work):
    """The server of `vendor/proc-macro-srv`, built and linked by ra-jit: where it is, and the
    line that says so."""
    server_root = os.path.realpath(SERVER)
    server = os.path.join(work, SERVER_BIN)
    crates = with_std([SERVER_BIN.replace("-", "_"),
                       *closure(metadata(server_root), SERVER_PACKAGE)])
    ran, took = ra_jit(server_root, crates, os.path.join(work, "server.o"), "--exe", server)
    linked(ran, "the server")
    return server, f"the server: built and linked by ra-jit in {took:.1f} s, {len(crates)} crates"


def main():
    if not sys.platform.startswith("linux"):
        print("not run: ra-jit links proc-macro libraries and the server on Linux so far")
        return
    work = scratch("ra-jit-macroserver-")
    passed = False
    try:
        server, said = built_server(work)
        lines = [said]
        if sys.argv[1:]:
            for name in sys.argv[1:]:
                lines.extend(app(name, work, server))
            print("\n".join(lines))
            passed = True
            return

        root = os.path.join(work, "macrouse")
        os.makedirs(os.path.join(root, "src"))
        with open(os.path.join(root, "Cargo.toml"), "w") as f:
            f.write(MANIFEST)
        with open(os.path.join(root, "src/main.rs"), "w") as f:
            f.write(SOURCE)
        run(["cargo", "generate-lockfile", "-q"], root, "cargo generate-lockfile")
        meta = metadata(root)
        libs = os.path.join(work, "libs")
        os.makedirs(libs)
        versions = []
        for package in meta["packages"]:
            if package["name"] not in MACRO_CRATES:
                continue
            name = lib_name(package)
            library = os.path.join(libs, f"lib{name}.so")
            crates = with_std([name, *closure(meta, package["name"])])
            ran, took = ra_jit(root, crates, os.path.join(work, f"{name}.o"), "--exe", library,
                               "--shared")
            linked(ran, package["name"])
            versions.append(f"{package['name']} {package['version']}")
            lines.append(f"{package['name']} {package['version']}: linked by ra-jit in "
                         f"{took:.1f} s")
        if len(versions) != len(MACRO_CRATES):
            raise SystemExit(f"FAIL: the package depends on {versions}, not each of "
                             f"{sorted(MACRO_CRATES)}")

        env = {"RA_JIT_PROC_MACRO_COMPARE": server, "RA_JIT_PROC_MACRO_LIBS": libs}
        # With its tests, whose `#[tokio::test]` is another attribute.
        ran, took = ra_jit(root, ["macrouse"], os.path.join(work, "macrouse.o"), "--cfg-test",
                           env=env)
        said = re.findall(r"compared with ra-jit's server: (\d+) calls \((\d+) derives, "
                          r"(\d+) attributes, (\d+) function-like\), (\d+) expanded otherwise, "
                          r"(\d+) failed by both, (\d+) not compared", ran.stderr)
        if not said:
            raise SystemExit(f"FAIL: the build compared nothing:\n{ran.stderr[-8000:]}")
        calls, derives, attrs, bangs, differed, failed, unloaded = map(int, said[-1])
        if differed or failed or unloaded or not (derives and attrs > 1 and bangs):
            raise SystemExit(f"FAIL: {calls} calls compared, {derives} derives, {attrs} "
                             f"attributes, {bangs} function-like, {differed} expanded "
                             f"otherwise, {failed} failed by both, {unloaded} not compared:\n"
                             f"{ran.stderr[-8000:]}")
        lines.append(f"the package: {calls} calls ({derives} derives, {attrs} attributes, "
                     f"{bangs} function-like) expanded by both servers, the same, "
                     f"in {took:.1f} s")
        lines.extend(wired(work, root, meta, server))
        print("\n".join(lines))
        print(f"PASS: ra-jit's server and libraries expand as the toolchain's, "
              f"{', '.join(versions)}")
        passed = True
    finally:
        if passed:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")


if __name__ == "__main__":
    main()
