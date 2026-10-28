#!/usr/bin/env python3
"""macroserver.py: the release's proc-macro server, built by blaze, expanding with the
libraries blaze links, held against the toolchain's server with rustc's (docs/no-rustc.md,
step J1).

blaze builds the server of `vendor/proc-macro-srv` (`--exe`), and each proc-macro crate a
package of serde, thiserror, tokio and clap depends on as a shared library with the list of
its macros in it (`--exe` with `--shared`, step J2). It then builds the package with
`BLAZE_PROC_MACRO_COMPARE` naming that server (crates/blaze-expand/src/compare.rs): each
call the toolchain's server expands with rustc's library, derives with helper attributes,
attributes and function-like macros, is expanded again by blaze's server with blaze's
library, and every output has to be the toolchain's, spans and all.

Then the package is built whole by blaze into its own target directory (`--own-target`) and
run, twice: once as before, where the load's `cargo check --compile-time-deps` has rustc build
the proc macros, and once with `BLAZE_PROC_MACRO_SERVER` naming the server, where blaze links
each of them (crates/blaze-cargo/src/cargo/own_macro.rs) and that server expands with them. Each
program has to print what cargo's does, and the proc-macro libraries in the target directory
have to be all rustc's, then all blaze's. Every build shares one store: the package whole,
which names the standard library's crates, has to refuse what the server's and libraries'
builds over the library's unit kept (#328).

With the names of bench apps (`APPS`, or `all` for each of them) it holds those instead. Each
is built whole by blaze into its own target directory twice, from an empty store each time:
with the server named, where blaze links every proc macro the load's cargo asks for and its
server expands with them, and without, where rustc's libraries are expanded by the toolchain's
server and every call is expanded again by blaze's server with blaze's library of the same
unit. Every call has to expand the same, every proc macro has to be blaze's, and the two
programs have to print the same. It says how many calls it compared and names each macro that
expanded otherwise or was not compared.

    python3 macroserver.py
    python3 macroserver.py nushell helix
    python3 macroserver.py all
"""
import glob, json, os, platform, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from allblaze import std_of
from difftest import BLAZE
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
# The name of the static blaze lists a proc-macro library's macros in.
LIST = b"BLAZE_PROC_MACROS"
# The bench apps it measures: the crate blaze is asked for and the program it links.
APPS = {"fd": ("fd", "fd"), "bat": ("bat", "bat"), "hyperfine": ("hyperfine", "hyperfine"),
        "just": ("just", "just"), "nushell": ("nu", "nu"), "helix": ("hx", "hx")}


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


def unit_graph(root):
    """What `cargo build` in `root` compiles, unit by unit: the graph of the build, not cargo
    metadata's resolve, which also has an optional dependency that only a weak feature
    (`borsh?/std`) names and so is never built: smol_str's borsh."""
    ran, _ = run(["cargo", "build", "--unit-graph", "-Z", "unstable-options"], root,
                 f"cargo build --unit-graph of {root}")
    return json.loads(ran.stdout)["units"]


def closure(meta, units, name):
    """The package called `name` and the libraries it links in the build: its units'
    dependencies, all the way down, but build scripts and proc-macro crates, which run while
    it is built. Each by its crate's name."""
    def skipped(unit):
        kinds = set(unit["target"]["kind"])
        return unit["mode"] == "run-custom-build" or bool({"custom-build", "proc-macro"} & kinds)
    ids = {p["id"] for p in meta["packages"] if p["name"] == name}
    starts = [i for i, u in enumerate(units)
              if u["pkg_id"] in ids and "custom-build" not in u["target"]["kind"]
              and u["mode"] != "run-custom-build"]
    if not starts:
        raise SystemExit(f"FAIL: the build's unit graph has no package {name}")
    seen, todo, crates = set(starts), list(starts), []
    while todo:
        unit = units[todo.pop()]
        for dep in unit["dependencies"]:
            index = dep["index"]
            if index in seen or skipped(units[index]):
                continue
            seen.add(index)
            todo.append(index)
            crate = units[index]["target"]["name"].replace("-", "_")
            if crate not in crates:
                crates.append(crate)
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


def blaze(root, crates, out, *extra, env=None):
    args = sum((["--crate", c] for c in crates), [])
    return run([BLAZE, root, *args, "--out", out, *extra], root, f"blaze {crates[0]}", env)


def linked(ran, what):
    said = re.search(r"^linked .* in (\S+), (\d+) functions in it blaze could not compile$",
                     ran.stderr, re.M)
    if not said:
        raise SystemExit(f"FAIL: {what}: blaze did not say it linked:\n{ran.stderr[-8000:]}")
    if said.group(2) != "0":
        raise SystemExit(f"FAIL: {what}: {said.group(2)} functions blaze could not compile:\n"
                         f"{ran.stderr[-8000:]}")


def libraries_in(target):
    """The shared libraries cargo built in `target`: in `deps/`, or, in the build directory's
    layout of cargo since nightly-2026-10-25, in each unit's `build/<package>/<hash>/out/`."""
    return [lib for at in ("deps", "out")
            for lib in glob.glob(os.path.join(target, "**", at, "lib*.so"), recursive=True)]


def built_macros(target):
    """The proc-macro libraries the load's cargo built in `target`, blaze's ones apart."""
    libraries = libraries_in(target)
    ours = {lib for lib in libraries if LIST in open(lib, "rb").read()}
    name = lambda lib: os.path.basename(lib)[3:].rsplit("-", 1)[0]
    return sorted(map(name, ours)), sorted(map(name, set(libraries) - ours))


def without_libc(target):
    """blaze's proc-macro libraries in `target` that do not name libc: glibc binds each libc
    symbol of such a library to its oldest version, a `realpath` that fails without a buffer."""
    read = {lib: open(lib, "rb").read() for lib in libraries_in(target)}
    return sorted(os.path.basename(lib) for lib, data in read.items()
                  if LIST in data and b"libc.so" not in data)


def wired(work, root, meta, units, server):
    """The package built whole by blaze and run, its proc macros built by the load's cargo in
    blaze's own target directory (`--own-target`): by rustc, and then by blaze with the
    server named, which expands with its libraries."""
    program = os.path.join(work, "macrouse-blaze")
    cargo_target = os.path.join(work, "cargo-target")
    ran, _ = run(["cargo", "run", "-q"], root, "cargo run", env={"CARGO_TARGET_DIR": cargo_target})
    expected = ran.stdout
    crates = with_std(["macrouse", *closure(meta, units, "macrouse")])
    lines, wanted = [], sorted(lib_name(p) for p in meta["packages"] if p["name"] in MACRO_CRATES)
    for named in (False, True):
        # A target directory of its own is seeded from cargo's, whose proc macros are rustc's.
        shutil.rmtree(os.path.join(root, "target"), ignore_errors=True)
        env = {"BLAZE_PROC_MACRO_SERVER": server if named else "0"}
        ran, took = blaze(root, crates, os.path.join(work, "wired.o"), "--own-target",
                           "--exe", program, env=env)
        how = "with the server named" if named else "without it"
        linked(ran, f"the package ({how})")
        got = run([program], root, f"the package ({how})")[0].stdout
        if got != expected:
            raise SystemExit(f"FAIL: the package ({how}) printed otherwise than cargo's:\n"
                             f"cargo:  {expected}\nblaze: {got}")
        # A load without blaze's proc macros has a directory of its own (#460).
        own = os.path.join(root, "target", "rust-analyzer" if named else "rust-analyzer-rustc")
        ours, theirs = built_macros(own)
        if (ours, theirs) != ((wanted, []) if named else ([], wanted)):
            raise SystemExit(f"FAIL: the load ({how}) built the proc macros {ours} by blaze and "
                             f"{theirs} by rustc, not each of {wanted} by "
                             f"{'blaze' if named else 'rustc'}:\n{ran.stderr[-8000:]}")
        bare = without_libc(own)
        if sys.platform == "linux" and bare:
            raise SystemExit(f"FAIL: blaze linked the proc macros {bare} without libc")
        lines.append(f"the package whole, its proc macros by {'blaze' if named else 'rustc'}: "
                     f"built and linked in {took:.1f} s, prints what cargo's does")
    return lines


def app(name, work, server):
    """A bench app of bench.py built whole by blaze into its own target directory, three
    times. First from an empty store with the server named, so the load's cargo has blaze
    link every proc macro and blaze's server expands with them; then again from that store
    into an empty target directory, where every library blaze linked is the store's; then
    from an empty store without the server, rustc's libraries expanded by the toolchain's
    server, and every call expanded again by blaze's server with the library blaze linked of
    the same unit (`BLAZE_PROC_MACRO_COMPARE`). The programs have to print the same. What
    each build took and what the comparison found, and the comparison's counts."""
    import bench
    spec, (crate, program) = bench.PROJECTS[name], APPS[name]
    root = os.path.join(work, name)
    run(["git", "clone", "-q", "--filter=blob:none", f"https://github.com/{spec['repo']}", root],
        work, f"git clone of {name}")
    run(["git", "-C", root, "checkout", "-q", "--detach", spec["rev"]], work, f"checkout of {name}")
    libs = os.path.join(work, f"{name}-libs")
    os.makedirs(libs)
    lines, printed, counts = [], [], None
    for build in ("ours", "again", "theirs"):
        named = build != "theirs"
        # A load without blaze's proc macros has a directory of its own (#460).
        own = os.path.join(root, "target", "rust-analyzer" if named else "rust-analyzer-rustc")
        shutil.rmtree(os.path.join(root, "target"), ignore_errors=True)
        store = os.path.join(work, f"{name}-store-{'ours' if named else 'theirs'}")
        env = {**spec.get("env", {}), "BLAZE_CACHE": store}
        if named:
            env["BLAZE_PROC_MACRO_SERVER"] = server
        else:
            env.update(BLAZE_PROC_MACRO_SERVER="0", BLAZE_PROC_MACRO_COMPARE=server,
                       BLAZE_PROC_MACRO_LIBS=libs)
        # The same file name in each, which a program's usage line prints.
        binary = os.path.join(work, f"{name}-{build}", program)
        os.makedirs(os.path.dirname(binary), exist_ok=True)
        ran, took = blaze(root, [crate], os.path.join(work, f"{name}.o"), "--own-target",
                           "--exe", binary, env=env)
        left = re.search(r"(\d+) functions in it blaze could not compile$", ran.stderr, re.M)
        ours, theirs = built_macros(own)
        # A library the store had is written with no build of it, which leaves a log.
        linked_now = {it for it in ours if glob.glob(
            os.path.join(own, "**", ".blaze-units", f"{it}-*", "*", "blaze.log"), recursive=True)}
        if build == "again" and linked_now:
            raise SystemExit(f"FAIL: {name}: the second build linked {sorted(linked_now)} again, "
                             "which the store had")
        if build == "ours":
            # A proc macro rustc compiled again, for a crate of rustc's that links it, left
            # blaze's library beside its record.
            for lib in [*libraries_in(own),
                        *glob.glob(os.path.join(own, "**", ".blaze-units", "*", "lib*.so"),
                                   recursive=True)]:
                if LIST in open(lib, "rb").read():
                    shutil.copy(lib, libs)
        said = subprocess.run([binary, *spec["run"]], cwd=root, capture_output=True, text=True,
                              timeout=60, env={**os.environ, **spec.get("env", {})})
        printed.append((said.returncode, said.stdout))
        start = len(lines)
        how = {"ours": "with the server", "again": "with the server and its store",
               "theirs": "without the server"}[build]
        lines.append(f"{name}, {how}: {took:.1f} s; the load's cargo built {len(theirs)} proc "
                     f"macros by rustc and {len(ours)} by blaze"
                     f"{f' ({len(ours) - len(linked_now)} from the store)' if named else ''}; "
                     f"{left.group(1) if left else '?'} functions left; `{program} "
                     f"{' '.join(spec['run'])}` exited {said.returncode}")
        if named:
            lines += [f"  rustc's: {it}" for it in fallbacks(own, theirs)]
        else:
            counts = compared(ran.stderr)
            lines.append(f"  {counts['line']}")
            lines += [f"  {it[:600]}" for it in re.findall(r"^proc macros: (`[^`]*`: .*)$",
                                                          ran.stderr, re.M)]
        print("\n".join(lines[start:]), flush=True)
    for at, which in ((0, "blaze's proc macros"), (1, "the store's proc macros")):
        if printed[at] != printed[2]:
            lines.append(f"{name}: the program with {which} printed otherwise:\n"
                         f"  blaze's: {printed[at]}\n  rustc's:  {printed[2]}")
            counts["printed otherwise"] = True
    shutil.rmtree(root, ignore_errors=True)
    return lines, counts


def compared(stderr):
    """What `BLAZE_PROC_MACRO_COMPARE` found, from the build's report."""
    said = re.findall(r"compared with blaze's server: (\d+) calls \((\d+) derives, "
                      r"(\d+) attributes, (\d+) function-like\), (\d+) expanded otherwise, "
                      r"(\d+) failed by both, (\d+) not compared"
                      r"(?: \(expanded otherwise: ([^)]*)\))?(?: \(not compared: ([^)]*)\))?",
                      stderr)
    if not said:
        raise SystemExit(f"FAIL: the build compared nothing:\n{stderr[-8000:]}")
    *numbers, otherwise, unloaded = said[-1]
    calls, derives, attrs, bangs, differed, failed, unloaded_calls = map(int, numbers)
    line = (f"{calls} calls compared ({derives} derives, {attrs} attributes, {bangs} "
            f"function-like): {calls - differed} the same, {differed} otherwise"
            f"{f' ({otherwise})' if otherwise else ''}, {failed} failed by both; "
            f"{unloaded_calls} not compared{f' ({unloaded})' if unloaded else ''}")
    return dict(calls=calls, derives=derives, attrs=attrs, bangs=bangs, differed=differed,
                failed=failed, unloaded=unloaded_calls, line=line)


def fallbacks(own, theirs):
    """Why each proc macro of `theirs` is rustc's: rustc compiled it again for a crate of
    rustc's that links it, or what blaze's build of it could not compile, or else the last
    line that build said."""
    said = []
    for name in theirs:
        units = os.path.join(own, "**", ".blaze-units", f"{name}-*")
        if glob.glob(os.path.join(units, "lib*.so"), recursive=True):
            said.append(f"{name}: blaze linked it, and rustc compiled it again for a crate of "
                        "rustc's that links it")
            continue
        logs = glob.glob(os.path.join(units, "*", "blaze.log"), recursive=True)
        lines = [l for log in logs for l in open(log).read().splitlines() if l.strip()]
        line = next((l for l in lines if "could not compile `" in l), lines[-1] if lines else None)
        # A chain of callees ends at what blaze could not compile.
        if line and len(line) > 600:
            line = f"{line[:200]} ... {line[-400:]}"
        said.append(f"{name}: {line or 'blaze did not build it'}")
    return said


def built_server(work):
    """The server of `vendor/proc-macro-srv`, built and linked by blaze: where it is, and the
    line that says so."""
    server_root = os.path.realpath(SERVER)
    server = os.path.join(work, SERVER_BIN)
    crates = with_std([SERVER_BIN.replace("-", "_"),
                       *closure(metadata(server_root), unit_graph(server_root),
                               SERVER_PACKAGE)])
    ran, took = blaze(server_root, crates, os.path.join(work, "server.o"), "--exe", server)
    linked(ran, "the server")
    return server, f"the server: built and linked by blaze in {took:.1f} s, {len(crates)} crates"


def main():
    if not sys.platform.startswith("linux"):
        print("not run: blaze links proc-macro libraries and the server on Linux so far")
        return
    work = scratch("blaze-macroserver-")
    # One store for the run: the server and the libraries are built over the standard
    # library's unit, and the package whole, which names its crates, must not take code from
    # the store that calls into the unit (#328). An app's builds have stores of their own.
    os.environ["BLAZE_CACHE"] = os.path.join(work, "store")
    passed = False
    try:
        server, said = built_server(work)
        lines = [said]
        if sys.argv[1:]:
            names = list(APPS) if sys.argv[1:] == ["all"] else sys.argv[1:]
            total, wrong = dict.fromkeys(("calls", "differed", "failed", "unloaded"), 0), []
            for name in names:
                try:
                    said, counts = app(name, work, server)
                except SystemExit as failed:
                    print(failed, flush=True)
                    lines.append(f"{name}: {str(failed)[:2000]}")
                    wrong.append(name)
                    continue
                lines.extend(said)
                for key in total:
                    total[key] += counts[key]
                if counts["differed"] or counts["unloaded"] or counts.get("printed otherwise"):
                    wrong.append(name)
            print("\n".join(lines))
            print(f"over {', '.join(names)}: {total['calls']} calls compared, "
                  f"{total['calls'] - total['differed']} the same, {total['differed']} otherwise, "
                  f"{total['failed']} failed by both, {total['unloaded']} not compared")
            if wrong:
                raise SystemExit(f"FAIL: blaze's server and libraries did not expand as the "
                                 f"toolchain's in {', '.join(wrong)}")
            print("PASS: every call expanded by blaze's server and libraries as the toolchain's")
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
        units = unit_graph(root)
        libs = os.path.join(work, "libs")
        os.makedirs(libs)
        versions = []
        for package in meta["packages"]:
            if package["name"] not in MACRO_CRATES:
                continue
            name = lib_name(package)
            library = os.path.join(libs, f"lib{name}.so")
            crates = with_std([name, *closure(meta, units, package["name"])])
            ran, took = blaze(root, crates, os.path.join(work, f"{name}.o"), "--exe", library,
                               "--shared")
            linked(ran, package["name"])
            versions.append(f"{package['name']} {package['version']}")
            lines.append(f"{package['name']} {package['version']}: linked by blaze in "
                         f"{took:.1f} s")
        if len(versions) != len(MACRO_CRATES):
            raise SystemExit(f"FAIL: the package depends on {versions}, not each of "
                             f"{sorted(MACRO_CRATES)}")

        env = {"BLAZE_PROC_MACRO_COMPARE": server, "BLAZE_PROC_MACRO_LIBS": libs}
        # With its tests, whose `#[tokio::test]` is another attribute.
        ran, took = blaze(root, ["macrouse"], os.path.join(work, "macrouse.o"), "--unit-test",
                           env=env)
        counts = compared(ran.stderr)
        calls, derives, attrs, bangs, differed, failed, unloaded = (
            counts[it] for it in ("calls", "derives", "attrs", "bangs", "differed", "failed",
                                  "unloaded"))
        if differed or failed or unloaded or not (derives and attrs > 1 and bangs):
            raise SystemExit(f"FAIL: {calls} calls compared, {derives} derives, {attrs} "
                             f"attributes, {bangs} function-like, {differed} expanded "
                             f"otherwise, {failed} failed by both, {unloaded} not compared:\n"
                             f"{ran.stderr[-8000:]}")
        lines.append(f"the package: {calls} calls ({derives} derives, {attrs} attributes, "
                     f"{bangs} function-like) expanded by both servers, the same, "
                     f"in {took:.1f} s")
        lines.extend(wired(work, root, meta, units, server))
        print("\n".join(lines))
        print(f"PASS: blaze's server and libraries expand as the toolchain's, "
              f"{', '.join(versions)}")
        passed = True
    finally:
        if passed:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")


if __name__ == "__main__":
    main()
