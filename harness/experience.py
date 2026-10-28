#!/usr/bin/env python3
"""experience.py: what a new user sees, step by step, on real projects: `blaze build`,
the same again, `run`, `test`, `check`, an edit and its build and run, `status` and `stop`,
each beside plain `cargo` on a copy of the same project, with what each step printed.

    python3 experience.py                          # every project, with the blaze on PATH
    python3 experience.py hello derives            # some of them
    python3 experience.py --install ARCHIVE ...    # install.sh with BLAZE_ARCHIVE, into a HOME of its own, first
    python3 experience.py --readme ...             # install.sh from https://ix.dev/blaze, as the README has it
    python3 experience.py --install A --rustup ... # a user with rustup: its toolchains are kept
    python3 experience.py --update ...             # write what each step printed as experience/<project>.txt
    python3 experience.py --list

It is a user's view: it runs `blaze` from `PATH`, sets no `BLAZE_*`
variable, and reads nothing but what the commands print and leave behind. A step fails the
run when it is a rough edge a user would trip on:

  - a command that exits other than as cargo's does (cargo exits 0 where blaze does not;
    where cargo fails and blaze does not, cargo's last line is noted and the run goes on);
  - a name blaze no longer has in anything printed (`DEAD_NAMES`, docs/rename.md);
  - a line that is not cargo's, the program's or the compiler's own, unless the project's
    transcript in `experience/` has it (a new message is looked at before it is accepted);
  - a build slower than cargo's of the same step, by more than `SLACK` (cold, no-op, edit);
  - a second no-op build that prints other than the first;
  - a cold build that shows a terminal nothing for more than `SILENT` seconds (it looks stuck);
  - an edit whose run does not print the edited output;
  - a blaze process left after `blaze stop`.

What each step printed, with times, paths and hashes taken out and cargo's own progress lines
(`Compiling`, `Downloaded`, ...) left out, has to be the project's `experience/<project>.txt`
to the byte; `--update` writes it. Results: `$TMPDIR/experience/<project>/report.md`.

Linux and macOS. `pyo3` needs python3; `sys` needs pkg-config, OpenSSL's headers and zlib's;
`wasm32` needs the wasm32-unknown-unknown target of blaze's toolchain.
"""
import argparse, glob, json, os, pty, re, select, shutil, signal, subprocess, sys, tempfile, threading, time, tty

HERE = os.path.dirname(os.path.abspath(__file__))
GOLDEN = os.path.join(HERE, "experience")
TIMEOUT = 3600
# How much slower than cargo a step may be before it is one: noise on a busy machine, and
# blaze's client starting.
SLACK = 1.10
SLACK_ABS = 0.15
# The longest a terminal may be shown nothing while a build goes on: longer looks stuck.
SILENT = 1.5


def toml(name, deps="", extra=""):
    return f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n{extra}\n[dependencies]\n{deps}'


DERIVES_MAIN = """use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value_t = 3)]
    count: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Point {
    x: i32,
    label: String,
}

fn said() -> &'static str {
    "before"
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    let p = Point { x: args.count as i32, label: dlib::label() };
    let json = serde_json::to_string(&p).unwrap();
    let back: Point = serde_json::from_str(&json).unwrap();
    let n = tokio::spawn(async move { back.x * 2 }).await.unwrap();
    println!("{json} {n} {}", said());
}

#[cfg(test)]
mod tests {
    #[test]
    fn says() {
        assert_eq!(super::said(), "before");
    }
}
"""

PYO3_LIB = """use pyo3::prelude::*;

#[pyfunction]
fn said() -> &'static str {
    pyext_core::said()
}

#[pymodule]
fn pyext(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(said, m)?)
}
"""

SYS_MAIN = """use std::io::Write;

fn said() -> &'static str {
    "before"
}

fn main() {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(b"hello hello hello hello").unwrap();
    let packed = z.finish().unwrap();
    let digest = openssl::sha::sha256(b"blaze");
    println!("zlib {} bytes, sha256 {:02x}{:02x}, openssl {}, {}", packed.len(), digest[0], digest[1],
             openssl::version::number() > 0, said());
}
"""

OBJCOPY_BUILD = r"""use std::{path::PathBuf, process::Command};

// As a firmware or eBPF project's build script does: the toolchain's llvm-objcopy, from
// rustc's sysroot (llvm-tools), run on an object of its own.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap();
    let out = Command::new(&rustc).args(["--print", "sysroot"]).output().unwrap();
    let sysroot = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    let host = std::env::var("HOST").unwrap();
    let tool = sysroot.join("lib/rustlib").join(&host).join("bin/llvm-objcopy");
    let status = Command::new(&tool).arg("--version").output()
        .unwrap_or_else(|e| panic!("running {}: {e}", tool.display()));
    assert!(status.status.success(), "{} failed: {}", tool.display(), String::from_utf8_lossy(&status.stderr));
    println!("cargo:rustc-env=OBJCOPY_SAID=ran");
}
"""

ANIM_DEPS = """base64 = "0.23.1"
block2 = "0.6.2"
bytemuck = { version = "1.25.2", features = ["derive"] }
cpal = "0.18.2"
crossterm = "0.29.0"
flate2 = "1.1.10"
libc = "0.2.191"
objc2 = "0.6.5"
objc2-app-kit = { version = "0.3.2", default-features = false, features = ["NSHapticFeedback", "NSScreen", "std"] }
objc2-core-foundation = { version = "0.3.2", default-features = false, features = ["std", "CFData", "CFCGTypes"] }
objc2-core-graphics = { version = "0.3.2", default-features = false, features = ["std", "CGImage", "CGDataProvider", "CGDirectDisplay"] }
objc2-foundation = { version = "0.3.2", default-features = false, features = ["std", "NSArray", "NSError"] }
pollster = "1.0.1"
svgtypes = "0.16.1"
taffy = "0.15.0"
tiny-skia = "0.12.0"
wgpu = { version = "30.0.1", default-features = false, features = ["metal", "wgsl"] }
"""

# anim's shape (a crate-local `OnceLock` static set from `main`, refused by the 10-20 build as
# "writable static `HOME` without a rustc symbol"), with its dependencies.
MACAPP_MAIN = """static HOME: std::sync::OnceLock<(u16, u16)> = std::sync::OnceLock::new();

fn detect() -> (u16, u16) {
    *HOME.get_or_init(|| {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        (w.min(80), h.min(24))
    })
}

fn said() -> &'static str {
    "before"
}

fn main() {
    let (w, h) = detect();
    let mut pixmap = tiny_skia::Pixmap::new(4, 4).unwrap();
    pixmap.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
    let _ = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pixmap.data());
    println!("{}x{} {} {}", w > 0, h > 0, encoded.len(), said());
}
"""

PROJECTS = {
    "hello": dict(files={
        "Cargo.toml": toml("hello"),
        "src/main.rs": 'fn main() {\n    println!("Hello, world!");\n}\n',
    }, edit=("src/main.rs", "Hello, world!", "Hello, blaze!"), says=("Hello, world!", "Hello, blaze!"), release=True),
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-c", "fn main", "crates/core/main.rs"],
        edit=("crates/core/main.rs", "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {",
              "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {\n    let _experience = 1u32;"),
        test=["--workspace"]),
    "fd": dict(
        repo="sharkdp/fd", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edit=("src/main.rs", "fn run() -> Result<ExitCode> {", "fn run() -> Result<ExitCode> {\n    let _experience = 1u32;")),
    "bat": dict(
        repo="sharkdp/bat", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edit=("src/bin/bat/main.rs", "fn run() -> Result<bool> {", "fn run() -> Result<bool> {\n    let _experience = 1u32;")),
    # A workspace of proc macros: serde's and clap's derives and tokio's attribute.
    "derives": dict(files={
        "Cargo.toml": '[workspace]\nmembers = ["app", "dlib"]\nresolver = "2"\n',
        "app/Cargo.toml": toml("app", 'clap = { version = "4", features = ["derive"] }\n'
                               'serde = { version = "1", features = ["derive"] }\nserde_json = "1"\n'
                               'tokio = { version = "1", features = ["rt", "macros"] }\ndlib = { path = "../dlib" }\n'),
        "app/src/main.rs": DERIVES_MAIN,
        "dlib/Cargo.toml": toml("dlib", 'serde = { version = "1", features = ["derive"] }\n'),
        "dlib/src/lib.rs": '#[derive(serde::Serialize, Default)]\npub struct Tag(pub u8);\n\n'
                           'pub fn label() -> String {\n    format!("tag{}", Tag(7).0)\n}\n',
    }, run=["-p", "app", "--", "--count", "4"], edit=("app/src/main.rs", '"before"\n}', '"after"\n}'),
        says=("before", "after"), release=True),
    # A new project's first dependency, before any lock file: `cargo new`, `cargo add itoa`.
    "newdeps": dict(files={
        "Cargo.toml": toml("newdeps", 'itoa = "1"\n'),
        "src/main.rs": 'fn main() {\n    println!("{}", itoa::Buffer::new().format(6 * 7));\n}\n',
    }, edit=("src/main.rs", "6 * 7", "6 * 8"), says=("42", "48"), unlocked=True),
    # A Python extension (cdylib) and the crate it calls.
    "pyo3": dict(files={
        "Cargo.toml": '[workspace]\nmembers = ["ext", "core"]\nresolver = "2"\n',
        "ext/Cargo.toml": toml("pyext", 'pyo3 = { version = "0.25", features = ["extension-module", "abi3-py38"] }\n'
                               'pyext_core = { path = "../core" }\n', '\n[lib]\ncrate-type = ["cdylib"]\n'),
        "ext/src/lib.rs": PYO3_LIB,
        "core/Cargo.toml": toml("pyext_core"),
        "core/src/lib.rs": 'pub fn said() -> &\'static str {\n    "before"\n}\n',
    }, run=None, edit=("core/src/lib.rs", '"before"', '"after"'), says=("before", "after"),
        python="import sys; sys.path.insert(0, '.'); import pyext; print(pyext.said())"),
    # A project pinned to an older stable: blaze builds it with its own toolchain and says so.
    "oldstable": dict(files={
        "rust-toolchain.toml": '[toolchain]\nchannel = "1.85.0"\n',
        "Cargo.toml": toml("oldstable", 'itoa = "1"\n'),
        "src/main.rs": 'fn main() {\n    println!("{}", itoa::Buffer::new().format(41 + 1));\n}\n',
    }, edit=("src/main.rs", "41 + 1", "41 + 2"), says=("42", "43")),
    # Build scripts that find C libraries with pkg-config: openssl-sys and libz-sys.
    "sys": dict(files={
        "Cargo.toml": toml("sysdeps", 'openssl = "0.10"\nflate2 = { version = "1", default-features = false, features = ["zlib"] }\n'),
        "src/main.rs": SYS_MAIN,
    }, edit=("src/main.rs", '"before"', '"after"'), says=("before", "after")),
    "wasm32": dict(files={
        "Cargo.toml": toml("wasmlib", "", '\n[lib]\ncrate-type = ["cdylib"]\n'),
        "src/lib.rs": '#[no_mangle]\npub extern "C" fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n',
    }, target="wasm32-unknown-unknown", run=None, test=None,
        edit=("src/lib.rs", "a + b", "a.wrapping_add(b)")),
    # A build script that runs the toolchain's llvm-objcopy (needs llvm-tools).
    "objcopy": dict(files={
        "Cargo.toml": toml("objcopy", "", 'build = "build.rs"\n'),
        "build.rs": OBJCOPY_BUILD,
        "src/main.rs": 'fn main() {\n    println!("{} before", env!("OBJCOPY_SAID"));\n}\n',
    }, edit=("src/main.rs", "before", "after"), says=("ran before", "ran after")),
    "macapp": dict(files={
        "Cargo.toml": toml("macapp", ANIM_DEPS, "").replace('edition = "2021"', 'edition = "2024"'),
        "src/main.rs": MACAPP_MAIN,
    }, edit=("src/main.rs", '"before"', '"after"'), says=("before", "after"), release=True, macos=True),
}

# What blaze was called before the rename (#270, docs/rename.md): a message, path or variable
# that says one is out of date.
DEAD_NAMES = re.compile(r"ra[-_]jit|RA_JIT|cargo[- ]blaze")  # rename: old name

# cargo's progress lines, whose order a parallel build does not fix.
PROGRESS = re.compile(r"^\s*(Compiling|Checking|Downloading|Downloaded|Updating|Locking|Adding|Fresh|"
                      r"Blocking|Building|Packaging|Unpacking|Fetching|Documenting|Removed|Updated|"
                      # blaze's own, of what its daemon is at, each shown only if it lasts a moment.
                      r"Loading|Resolving|Linking)\b")
OWN = re.compile(r"^\s*(Finished|Running|Executable|Doc-tests|running \d+ tests?|test result:|test \S+ \.\.\.|"
                 r"warning: .*generated \d+ warning)")


def normalized(text, where):
    for path, name in where:
        text = text.replace(path, name)
    text = re.sub(r"\b\d+(\.\d+)?\s?(ms|s|µs|us)\b", "<t>", text)
    text = re.sub(r"\b[0-9a-f]{16,}\b", "<hash>", text)
    text = re.sub(r"\bpid \d+", "pid <n>", text)
    text = re.sub(r"\b\d+(\.\d+)? ?(MB|GB|KB|MiB|GiB|KiB|B)\b", "<size>", text)
    lines = [line.rstrip() for line in text.splitlines() if not PROGRESS.match(line)]
    return "\n".join(lines)


def on_terminal(argv, cwd, env, timeout):
    """Runs argv with standard error a terminal, as a user's is: its exit, standard output,
    standard error with what was written over taken out, and the longest time it wrote nothing."""
    master, slave = pty.openpty()
    tty.setraw(slave)
    start = time.monotonic()
    p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=slave)
    os.close(slave)
    out = []
    reader = threading.Thread(target=lambda: out.append(p.stdout.read()))
    reader.start()
    raw, last, silent = b"", start, 0.0
    while True:
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(master, 65536)
            except OSError:  # Linux: every process that had it closed it
                chunk = b""
            if not chunk:
                break
            now = time.monotonic()
            silent, last = max(silent, now - last), now
            raw += chunk
        elif p.poll() is not None or time.monotonic() - start > timeout:
            break
    timed_out = p.poll() is None
    if timed_out:
        p.kill()
    code = "timeout" if timed_out else p.wait()
    silent = max(silent, time.monotonic() - last)
    os.close(master)
    reader.join()
    err = raw.decode(errors="replace")
    err = re.sub(r"\x1b\[[0-9;]*m", "", err)
    err = re.sub(r"\r[^\r\n]*\x1b\[K", "", err).replace("\x1b[K", "").replace("\r", "")
    return code, (out[0] if out else b"").decode(errors="replace"), err, silent


class Step:
    def __init__(self, name, argv, cwd, env, timeout=TIMEOUT, terminal=False):
        start = time.monotonic()
        self.silent = None
        if terminal:
            self.code, self.out, self.err, self.silent = on_terminal(argv, cwd, env, timeout)
        else:
            try:
                done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
                self.code, self.out, self.err = done.returncode, done.stdout, done.stderr
            except subprocess.TimeoutExpired as e:
                self.code, self.out, self.err = "timeout", e.stdout or "", (e.stderr or "") + f"\n(timed out after {timeout} s)"
                if isinstance(self.out, bytes):
                    self.out, self.err = self.out.decode(errors="replace"), self.err.decode(errors="replace")
        self.secs = time.monotonic() - start
        self.name, self.argv = name, argv


def plain_cargo(env):
    """The blaze is compared with, and its environment: the user's, else, on a machine
    with no Rust, the one blaze downloaded, with its toolchain first on PATH so that it finds
    its rustc."""
    users = shutil.which("cargo", path=env["PATH"])
    if users:
        return users, env
    own = sorted(glob.glob(os.path.join(env["HOME"], ".blaze", "toolchains", "*", "bin", "cargo")))
    if not own:
        return None, env
    return own[-1], {**env, "PATH": os.path.dirname(own[-1]) + os.pathsep + env["PATH"]}


def fetch(spec, into, env):
    os.makedirs(into)
    if "files" in spec:
        for path, text in spec["files"].items():
            os.makedirs(os.path.dirname(os.path.join(into, path)) or into, exist_ok=True)
            with open(os.path.join(into, path), "w") as f:
                f.write(text)
        # A project someone has worked on has its lock file; `newdeps` is one that has none yet.
        has_deps = any(re.search(r"\[dependencies\]\n\S", text) for text in spec["files"].values())
        cargo, cargo_env = plain_cargo(env)
        if has_deps and cargo and not spec.get("unlocked"):
            subprocess.run([cargo, "generate-lockfile", "--quiet"], cwd=into, env=cargo_env, check=True)
        return
    cache = os.path.join(tempfile.gettempdir(), "experience-src", spec["rev"])
    if not os.path.isdir(cache):
        subprocess.run(["git", "init", "-q", cache], check=True)
        subprocess.run(["git", "-C", cache, "fetch", "-q", "--depth", "1",
                        f"https://github.com/{spec['repo']}", spec["rev"]], check=True)
        subprocess.run(["git", "-C", cache, "checkout", "-q", "FETCH_HEAD"], check=True)
    shutil.rmtree(into)
    shutil.copytree(cache, into, ignore=shutil.ignore_patterns(".git"))


def edit(root, spec, undo=False):
    path, old, new = spec["edit"]
    if undo:
        old, new = new, old
    with open(os.path.join(root, path)) as f:
        text = f.read()
    assert old in text, f"{path} has no {old!r}"
    with open(os.path.join(root, path), "w") as f:
        f.write(text.replace(old, new, 1))


def blaze_processes():
    # By the program each runs, not by its arguments: a shell whose script names an archive
    # (`blaze-commit-...tar.gz`) is not blaze's. macOS's `pgrep -a` means ancestors, not arguments.
    out = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    left = []
    for line in out.splitlines():
        pid, _, command = line.strip().partition(" ")
        program = os.path.basename(command.split(" ", 1)[0]) if command else ""
        if re.fullmatch(r"(cargo-)?(blaze|ra-jit)(-[\w-]+)?", program):  # rename: old name
            left.append(f"{pid} {command[:160]}")
    return left


def flow(name, spec, work, env):
    """Every step, blaze's and cargo's, and the rough edges in them."""
    target = ["--target", spec["target"]] if spec.get("target") else []
    run_args = spec.get("run", [])
    blaze_dir, cargo_dir = os.path.join(work, name), os.path.join(work, name + "-cargo")
    for d in (blaze_dir, cargo_dir):
        shutil.rmtree(d, ignore_errors=True)
        fetch(spec, d, env)
    steps, rough = [], []
    front = ["blaze"]

    def blaze(step, *args, **kw):
        s = Step(step, [*front, *args], blaze_dir, env, **kw)
        steps.append(s)
        return s

    def cargo(step, *args):
        plain, plain_env = plain_cargo(env)
        return Step(step, [plain or "cargo", *args], cargo_dir, plain_env)

    def run_python(root, profile_dir):
        lib = os.path.join(root, "target", profile_dir, "libpyext.dylib" if sys.platform == "darwin" else "libpyext.so")
        if not os.path.exists(lib):
            return f"(no {os.path.relpath(lib, root)})"
        shutil.copy(lib, os.path.join(root, "pyext.so"))
        done = subprocess.run([sys.executable, "-c", spec["python"]], cwd=root, capture_output=True, text=True)
        return done.stdout.strip() or done.stderr.strip()

    cold = blaze("build (cold)", "build", *target, terminal=True)
    c_cold = cargo("build (cold)", "build", *target)
    noop = blaze("build (no-op)", "build", *target)
    noop2 = blaze("build (no-op again)", "build", *target)
    c_noop = cargo("build (no-op)", "build", *target)
    pairs = [(cold, c_cold), (noop, c_noop)]
    sep = ["--"] if run_args and run_args[0] != "-p" else []
    if run_args is not None:
        pairs.append((blaze("run", "run", *target, *sep, *run_args), cargo("run", "run", *target, *sep, *run_args)))
    if spec.get("test", []) is not None:
        pairs.append((blaze("test", "test", *target, *spec.get("test", [])), cargo("test", "test", *target, *spec.get("test", []))))
    pairs.append((blaze("check", "check", *target), cargo("check", "check", *target)))
    edit(blaze_dir, spec)
    edit(cargo_dir, spec)
    edited = blaze("build (edit)", "build", *target)
    c_edited = cargo("build (edit)", "build", *target)
    pairs.append((edited, c_edited))
    says = spec.get("says")
    if says and run_args is not None:
        ran = blaze("run (edited)", "run", *target, *(["--"] if run_args and run_args[0] != "-p" else []), *run_args)
        if says[1] not in ran.out:
            rough.append(("run (edited)", f"the run after the edit printed {ran.out.strip()!r}, not {says[1]!r}", "P0"))
    if says and spec.get("python"):
        got = run_python(blaze_dir, "debug")
        if got != says[1]:
            rough.append(("python (edited)", f"the extension says {got!r} after the edit, not {says[1]!r}", "P0"))
    if spec.get("release") and run_args is not None:
        ran = blaze("run --release", "run", "--release", *target, *run_args)
        if says and says[1] not in ran.out:
            rough.append(("run --release", f"printed {ran.out.strip()!r}, not {says[1]!r}", "P0"))
    blaze("status", "status")
    blaze("stop", "stop")
    time.sleep(1)
    left = blaze_processes()
    if left:
        rough.append(("stop", "blaze processes left after `blaze stop`: " + "; ".join(left), "P1"))

    where = [(blaze_dir, "$PROJECT"), (os.path.realpath(blaze_dir), "$PROJECT"), (env["HOME"], "$HOME")]
    if pairs and pairs[0][1].code != 0:
        said = (pairs[0][1].err + pairs[0][1].out).strip().splitlines()
        rough = [(pairs[0][1].name, "cargo fails here too, so this says nothing of blaze: "
                  + (said[-1] if said else ""), "env")]
        return steps, rough, where, [theirs for _, theirs in pairs]
    last = lambda step: ((step.err + step.out).strip().splitlines() or [""])[-1]
    for ours, theirs in pairs:
        if ours.code == 0 and theirs.code != 0:
            rough.append((ours.name, f"cargo fails here (exit {theirs.code}), blaze does not: {last(theirs)}", "env"))
        elif ours.code != theirs.code:
            rough.append((ours.name, f"exit {ours.code}, cargo's {theirs.code}: {last(ours)}", "P0"))
        elif ours.code == 0 and ours.secs > theirs.secs * SLACK + SLACK_ABS:
            rough.append((ours.name, f"{ours.secs:.2f} s, cargo {theirs.secs:.2f} s", "P0"))
    for s in steps:
        if s.code != 0 and s.name not in [ours.name for ours, _ in pairs]:
            rough.append((s.name, f"exit {s.code}", "P0"))
        for line in (s.out + s.err).splitlines():
            if DEAD_NAMES.search(line):
                rough.append((s.name, f"says a name blaze no longer has: {line.strip()}", "P2"))
            # crates/blaze-ui says it at the end when the build spent UNNAMED_NS or more at no step.
            if "with no step named" in line:
                rough.append((s.name, f"a second or more of it is at no step: {line.strip()}", "P1"))
    for ours, theirs in pairs:
        compiled = lambda step: sorted(l.split()[1] for l in step.err.splitlines() if l.strip().startswith("Compiling "))
        if compiled(ours) and not compiled(theirs) and ours.code == 0:
            rough.append((ours.name, f"says Compiling {', '.join(compiled(ours))}, where cargo compiles nothing", "P1"))
    if cold.silent is not None and cold.silent > SILENT:
        rough.append((cold.name, f"a terminal is shown nothing for {cold.silent:.1f} s", "P1"))
    if normalized(noop.err, where) != normalized(noop2.err, where):
        rough.append(("build (no-op again)", "prints other than the first no-op", "P2"))
    return steps, rough, where, [theirs for _, theirs in pairs]


def transcript(steps, where):
    parts = []
    for s in steps:
        parts.append(f"$ {' '.join(s.argv)}    # {s.name}, exit {s.code}")
        text = normalized(s.err + s.out, where)
        if text:
            parts.append(text)
    return "\n".join(parts) + "\n"


def install(home, archive, readme, rustup):
    os.makedirs(home, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("BLAZE", "CARGO", "RUSTUP"))}
    if rustup:
        # A user who has rustup: its toolchains stay where they are.
        env["RUSTUP_HOME"] = os.environ.get("RUSTUP_HOME") or os.path.expanduser("~/.rustup")
    env["HOME"] = home
    if readme:
        script = subprocess.run(["curl", "-fsSL", "https://ix.dev/blaze"], capture_output=True, text=True, check=True).stdout
    else:
        env["BLAZE_ARCHIVE"] = os.path.abspath(archive)
        with open(os.path.join(HERE, "..", "install.sh")) as f:
            script = f.read()
    done = subprocess.run(["sh"], input=script, env=env, capture_output=True, text=True)
    env.pop("BLAZE_ARCHIVE", None)
    env["PATH"] = os.path.join(home, ".cargo", "bin") + os.pathsep + env["PATH"]
    return env, done


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("projects", nargs="*")
    parser.add_argument("--install", metavar="ARCHIVE")
    parser.add_argument("--readme", action="store_true")
    parser.add_argument("--rustup", action="store_true", help="with --install or --readme: keep rustup's toolchains")
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "experience"))
    args = parser.parse_args()
    if args.list:
        print("\n".join(PROJECTS))
        return 0
    names = args.projects or [n for n, it in PROJECTS.items() if not it.get("macos") or sys.platform == "darwin"]
    unknown = [n for n in names if n not in PROJECTS]
    if unknown:
        sys.exit(f"experience.py: no project {', '.join(unknown)}; --list names them")
    os.makedirs(args.work, exist_ok=True)
    env = dict(os.environ)
    if args.install or args.readme:
        env, done = install(os.path.join(args.work, "home"), args.install, args.readme, args.rustup)
        with open(os.path.join(args.work, "install.txt"), "w") as f:
            f.write(done.stdout + done.stderr)
        print(f"install: exit {done.returncode}\n{done.stdout}{done.stderr}")
        if done.returncode != 0:
            return 1
    failed = False
    summary = []
    for name in names:
        spec = PROJECTS[name]
        print(f"== {name}", flush=True)
        steps, rough, where, cargo_steps = flow(name, spec, args.work, env)
        text = transcript(steps, where)
        golden = os.path.join(GOLDEN, name + ".txt")
        if args.update:
            os.makedirs(GOLDEN, exist_ok=True)
            with open(golden, "w") as f:
                f.write(text)
        elif os.path.exists(golden):
            with open(golden) as f:
                want = f.read()
            if want != text:
                rough.append(("transcript", f"differs from experience/{name}.txt (diff in report.md)", "P2"))
        report = [f"# {name}\n", "| step | exit | blaze s | cargo s |", "|---|---|---:|---:|"]
        by_name = {c.name: c for c in cargo_steps}
        for s in steps:
            c = by_name.get(s.name)
            report.append(f"| {s.name} | {s.code} | {s.secs:.2f} | {f'{c.secs:.2f}' if c else ''} |")
        report += ["", "## Rough edges", ""] + [f"- **{sev}** {step}: {what}" for step, what, sev in rough]
        report += ["", "## What it printed", "", "```", "\n".join(
            f"$ {' '.join(s.argv)}    # {s.name}, exit {s.code}, {s.secs:.2f} s\n{s.err}{s.out}" for s in steps), "```"]
        os.makedirs(os.path.join(args.work, name), exist_ok=True)
        with open(os.path.join(args.work, name + ".report.md"), "w") as f:
            f.write("\n".join(report) + "\n")
        for step, what, sev in rough:
            print(f"  {sev} {step}: {what}")
        summary.append({"project": name, "steps": [{"name": s.name, "exit": s.code, "secs": round(s.secs, 3)} for s in steps],
                        "cargo": [{"name": c.name, "exit": c.code, "secs": round(c.secs, 3)} for c in cargo_steps],
                        "rough": rough})
        failed |= any(sev != "env" for _, _, sev in rough)
    with open(os.path.join(args.work, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    print("experience: " + ("rough edges above" if failed else "clean"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
