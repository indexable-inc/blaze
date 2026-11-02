#!/usr/bin/env python3
"""soakrows.py: the files and toolchains a daemon meets that a test workspace does not have.

    soakrows.py [ROW...] [--work DIR] [--other-fs DIR] [--keep]

Each row copies the `soak/` workspace (a build script, three kinds of proc macro, a program
and its tests), has a daemon of its own load it, does the thing the row is named for, and
then compares `blaze run` (or `test`) with blaze's build of the same tree from nothing
(`blaze_cli.Scratch`), and the program with what the edit says it prints. A row is right
when the two agree at every comparison, the program says what it should, and neither the
daemon nor a worker logged a panic; who built each one, the daemon or cargo with the reason
blaze gave, is in the table. A build cargo did in place of the daemon is right: that is the
contract.

Writes SOAKROWS.md next to this file.
"""
import argparse, glob, os, re, shutil, signal, subprocess, sys, tempfile, time

from blaze_cli import HERE, BLAZE, TOOLCHAIN, Scratch, Side, base_env, difference, how_built, program_output, test_results, wait_warm

GIT = ["git", "-c", "user.name=blaze", "-c", "user.email=blaze@localhost", "-c", "advice.detachedHead=false"]
OTHER_TOOLCHAIN = "1.94.0"
# The pinned nightly CI uses for borrowck_ui.py.
NIGHTLY = TOOLCHAIN
MARK = re.compile(r"pub fn mark\(\) -> u64 \{ \d+ \}")


class Wrong(Exception):
    pass


class NotRun(Exception):
    """What a row needs that this machine does not have."""


class Row:
    """One workspace, the daemon that holds it, and what was compared."""

    def __init__(self, work, name, env=None, at=None):
        self.dir = os.path.join(work, name)
        self.ws = os.path.realpath(at or os.path.join(self.dir, "ws"))
        self.home = os.path.join(self.dir, "home")
        self.store = os.path.join(self.home, "v2", "store")
        os.makedirs(self.dir, exist_ok=True)
        self.env = dict(base_env(self.home), **(env or {}))
        self.built, self.notes, self.mark = [], [], 1000
        self.copy(self.ws)

    def copy(self, ws):
        shutil.copytree(os.path.join(HERE, "soak"), ws, ignore=shutil.ignore_patterns("target*", "Cargo.lock"))
        with open(os.path.join(ws, "core/src/lib.rs"), "a") as lib:
            lib.write("\npub fn mark() -> u64 { 1000 }\n")
        main = os.path.join(ws, "app/src/main.rs")
        text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("mark {}", soak_core::mark());', 1)
        open(main, "w").write(text)
        open(os.path.join(ws, ".gitignore"), "w").write("target\ntarget-scratch\n")
        self.git("init", "-q", cwd=ws), self.git("add", ".", cwd=ws), self.git("commit", "-qm", "first", cwd=ws)

    def git(self, *args, cwd=None):
        return subprocess.run([*GIT, *args], cwd=cwd or self.ws, check=True, capture_output=True, text=True)

    def path(self, *parts, ws=None):
        return os.path.join(ws or self.ws, *parts)

    def next_mark(self):
        self.mark += 1
        return f"pub fn mark() -> u64 {{ {self.mark} }}"

    def edit(self, file=None, write=None):
        """Gives `mark` of the file another value of the same length, written by `write`."""
        file = file or self.path("core/src/lib.rs")
        text = MARK.sub(self.next_mark(), open(file).read())
        (write or in_place)(file, text)

    def warm(self, ws=None, env=None):
        side = Side(ws or self.ws, dict(self.env, **(env or {})), True)
        _, ran = side.cargo("build")
        if ran.returncode:
            raise Wrong(f"the first build failed: {ran.stderr[-300:]}")
        if "daemon: not running" in side.cargo("status")[1].stdout:
            raise Wrong(f"no daemon after the first build: {ran.stderr[-300:]}")
        _, states = wait_warm(side, os.path.realpath(ws or self.ws))
        return states

    def both(self, label, command="run", ws=None, env=None, args=(), want=None, fails=False):
        """Compares `blaze COMMAND` with blaze's `COMMAND` of the tree from nothing, and has
        the program say `want`. It has to exit 0, or other than 0 when it `fails`."""
        ws, env = ws or self.ws, dict(self.env, **(env or {}))
        _, ours = Side(ws, env, True).cargo(command, *args, timeout=1800)
        _, fresh = Scratch(ws, env).cargo(command, *args, timeout=1800)
        shutil.rmtree(self.path("target-scratch", ws=ws), ignore_errors=True)
        read = test_results if command == "test" else program_output
        differs = difference(read(ours), read(fresh))
        how = how_built(ours.stderr)
        self.built.append(f"{label}: {how}")
        if differs:
            raise Wrong(f"{label} ({how}): {differs}")
        if (ours.returncode != 0) != fails:
            raise Wrong(f"{label} ({how}): exits {ours.returncode} from nothing too: {ours.stderr[-300:]}")
        if want and want not in ours.stdout:
            raise Wrong(f"{label}: both say the same, and neither says `{want}`: {ours.stdout[-200:]}")
        return how, ours

    def status(self, env=None):
        text = Side(self.ws, dict(self.env, **(env or {})), True).cargo("status")[1].stdout
        pid = re.search(r"^daemon: pid (\d+)", text, re.M)
        return (int(pid[1]) if pid else None), [int(p) for p in re.findall(r"worker: pid (\d+)", text)], text

    def panics(self):
        logs = [os.path.join(self.home, "daemon.log"), *glob.glob(os.path.join(self.home, "**", "log"), recursive=True)]
        found = []
        for log in logs:
            try:
                found += [f"{log}: {l}" for l in open(log, errors="replace").read().splitlines() if "panicked" in l]
            except OSError:
                pass
        return found

    def stop(self, env=None):
        Side(self.ws, dict(self.env, **(env or {})), True).cargo("stop")
        pid = self.status(env)[0]
        try:
            os.kill(pid or 0, signal.SIGKILL) if pid else None
        except ProcessLookupError:
            pass


def in_place(file, text):
    """As an editor that truncates the file and writes it again."""
    with open(file, "w") as out:
        out.write(text)


def by_rename(file, text):
    """As an editor that writes another file and renames it over this one."""
    other = file + ".swp~"
    with open(other, "w") as out:
        out.write(text)
        out.flush()
        os.fsync(out.fileno())
    os.replace(other, file)


def same_stamp(file, text):
    """Written with the size and the time of modification it had."""
    was = os.stat(file)
    in_place(file, text)
    os.utime(file, ns=(was.st_atime_ns, was.st_mtime_ns))


def symlinked_file(row):
    lib, shared = row.path("core/src/lib.rs"), os.path.join(row.dir, "shared")
    os.makedirs(shared)
    shutil.move(lib, os.path.join(shared, "one.rs"))
    os.symlink(os.path.join(shared, "one.rs"), lib)
    row.warm()
    row.both("as loaded")
    row.edit(os.path.join(shared, "one.rs"))
    row.both("what it points at edited", want=f"mark {row.mark}")
    text = MARK.sub(row.next_mark(), open(os.path.join(shared, "one.rs")).read())
    in_place(os.path.join(shared, "two.rs"), text)
    os.remove(lib)
    os.symlink(os.path.join(shared, "two.rs"), lib)
    row.both("pointed at another file", want=f"mark {row.mark}")


def symlinked_directory(row):
    src, shared = row.path("core/src"), os.path.join(row.dir, "shared-src")
    shutil.move(src, shared)
    os.symlink(shared, src)
    row.warm()
    row.both("as loaded")
    row.edit(os.path.join(shared, "lib.rs"))
    row.both("a file under it edited", want=f"mark {row.mark}")
    row.edit(row.path("core/src/lib.rs"), by_rename)
    row.both("edited through the link", want=f"mark {row.mark}")


def symlinked_root(row):
    link = os.path.join(row.dir, "link")
    os.symlink(row.ws, link)
    args = ("--manifest-path", os.path.join(link, "Cargo.toml"))
    env = {"PWD": link}
    row.both("cold through the link", args=args, ws=link, env=env)
    wait_warm(Side(link, row.env, True), row.ws)
    row.notes.append("status after a build through the link:\n" + row.status()[2])
    row.edit()
    row.both("edited, through the link", args=args, ws=link, env=env, want=f"mark {row.mark}")
    row.edit(os.path.join(link, "core/src/lib.rs"))
    row.both("edited through the link, by the real path", want=f"mark {row.mark}")
    row.edit()
    row.both("again through the link", args=args, ws=link, env=env, want=f"mark {row.mark}")


def save_by_rename(row):
    row.warm()
    for round in range(4):
        row.edit(write=by_rename)
        row.both(f"renamed over, {round}", want=f"mark {row.mark}")


def save_by_truncate(row):
    row.warm()
    for round in range(4):
        row.edit(write=in_place)
        row.both(f"truncated and written, {round}", want=f"mark {row.mark}")


def twice_in_a_tick(row):
    row.warm()
    for round in range(3):
        row.edit()
        Side(row.ws, row.env, True).cargo("build")
        row.edit()
        row.both(f"written again at once, {round}", want=f"mark {row.mark}")
    # The same size and the same time: nothing but the bytes says it changed, and a build
    # that misses it is stale.
    row.edit(write=same_stamp)
    row.both("same size and time", want=f"mark {row.mark}")


def checkout_of_thousands(row, count=3000):
    def branch(name, add):
        row.git("checkout", "-q", "-b", name, "master")
        os.makedirs(row.path("app/src/many"), exist_ok=True)
        for at in range(count):
            in_place(row.path(f"app/src/many/m{at}.rs"), f"pub fn f() -> u64 {{ {at + add} }}\n")
        mods = "".join(f"pub mod m{at};\n" for at in range(count))
        all_of = ", ".join(f"m{at}::f" for at in range(count))
        in_place(row.path("app/src/many/mod.rs"), f"{mods}\npub fn total() -> u64 {{\n    let all: [fn() -> u64; {count}] = [{all_of}];\n    all.iter().map(|f| f()).sum()\n}}\n")
        main = open(row.path("app/src/main.rs")).read()
        in_place(row.path("app/src/main.rs"), "mod many;\n" + main.replace("fn main() {", 'fn main() {\n    println!("many {}", many::total());'))
        row.git("add", "."), row.git("commit", "-qm", name)
        return sum(range(count)) + add * count

    row.git("branch", "-M", "master")
    one, two = branch("one", 0), branch("two", 1)
    row.git("checkout", "-q", "master")
    row.warm()
    row.both("before")
    for name, total in (("one", one), ("two", two), ("one", one)):
        row.git("checkout", "-q", name)
        row.both(f"checkout of {count} files, {name}", want=f"many {total}")
    row.git("checkout", "-q", "master")
    row.both("checkout that removes them")


def case_only_rename(row):
    in_place(row.path("core/src/helper.rs"), "pub fn help() -> u64 { 5 }\n")
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib.replace("pub fn mark() -> u64 { 1000 }", "mod helper;\npub fn mark() -> u64 { 1000 + helper::help() }"))
    row.warm()
    row.both("as loaded", want="mark 1005")
    os.rename(row.path("core/src/helper.rs"), row.path("core/src/Helper.rs"))
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib.replace("mod helper;", '#[path = "Helper.rs"]\nmod helper;'))
    in_place(row.path("core/src/Helper.rs"), "pub fn help() -> u64 { 6 }\n")
    row.both("renamed in case only", want="mark 1006")
    os.rename(row.path("core/src/Helper.rs"), row.path("core/src/helper.rs"))
    in_place(row.path("core/src/lib.rs"), lib)
    in_place(row.path("core/src/helper.rs"), "pub fn help() -> u64 { 7 }\n")
    row.both("renamed back", want="mark 1007")


def worktree_recreated(row):
    row.git("branch", "-M", "master")
    for name in ("one", "two", "three"):
        row.git("checkout", "-q", "-b", name, "master")
        row.edit()
        row.git("commit", "-qam", name)
    row.git("checkout", "-q", "master")
    tree = os.path.join(row.dir, "tree")
    row.warm()
    row.git("worktree", "add", "-q", tree, "one")
    row.warm(ws=tree)
    row.both("the worktree", ws=tree, want="mark 1001")
    row.git("worktree", "remove", "--force", tree)
    row.git("worktree", "add", "-q", tree, "two")
    row.both("removed and added again at another commit", ws=tree, want="mark 1002")
    wait_warm(Side(tree, row.env, True), tree)
    shutil.rmtree(tree)
    row.git("worktree", "prune")
    time.sleep(2.5)
    row.notes.append("status with the worktree deleted:\n" + row.status()[2])
    row.git("worktree", "add", "-q", tree, "three")
    row.both("deleted and added again", ws=tree, want="mark 1003")
    wait_warm(Side(tree, row.env, True), tree)
    row.edit(os.path.join(tree, "core/src/lib.rs"))
    how, _ = row.both("edited after that", ws=tree, want=f"mark {row.mark}")
    row.both("the first checkout still", want="mark 1000")


def worktrees_built_apart(row):
    """Two worktrees whose libraries rustc built from different source: the one's holds code
    of the standard library for a type of its own that the program calls too, and the other's
    does not. The worktree asked second is read as the first."""
    kept = "\npub fn kept(count: usize) -> usize {\n    let mut all: Vec<Reading> = Vec::new();\n    for _ in 0..count {\n        all.push(Reading::default());\n    }\n    all.len()\n}\n"
    main = row.path("app/src/main.rs")
    text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("kept {}", kept(std::env::args().count() + 4));', 1)
    open(main, "w").write(text + kept.replace("pub fn", "fn").replace("Reading", "soak_core::Reading"))
    row.git("commit", "-qam", "the program keeps readings")
    row.git("branch", "-M", "master")
    row.git("checkout", "-q", "-b", "one", "master")
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write(kept)
    row.git("commit", "-qam", "the library keeps them too")
    row.git("checkout", "-q", "master")
    tree = os.path.join(row.dir, "tree")
    row.git("worktree", "add", "-q", tree, "one")
    row.warm(ws=tree)
    row.both("the worktree whose library keeps readings", ws=tree, want="kept 5")
    row.warm()
    def asked_after_the_other(label, ws, file):
        """The first build of one after the other may fail to link, which is said with what
        the linker lacked; the daemon loads it by itself once cargo has built it again."""
        how, _ = row.both(label, ws=ws, want="kept 5")
        if "link failed" in how and "undefined symbol" not in how:
            raise Wrong(f"refused without what the linker lacked: {how}")
        row.warm(ws=ws)
        row.edit(file)
        row.both(f"{label}, edited", ws=ws, want=f"mark {row.mark}")
        row.warm(ws=ws)
        for _ in range(3):
            row.edit(file)
            how, _ = row.both(f"{label}, edited again", ws=ws, want=f"mark {row.mark}")
            if how != "daemon":
                raise Wrong(f"an edit was not built by the daemon: {how}")

    asked_after_the_other("the one whose library does not", row.ws, None)
    asked_after_the_other("the first", tree, os.path.join(tree, "core/src/lib.rs"))


def worktrees_that_run_other_code(row):
    """Two worktrees whose proc macro and build script data differ, in one daemon: cargo
    names what each builds of them alike, so neither may be built where the other was. The
    files of the one asked second are the older ones, which is when cargo would take what
    the first built as fresh."""
    row.git("branch", "-M", "master")
    tree = os.path.join(row.dir, "tree")
    row.git("worktree", "add", "-q", tree, "-b", "one")
    time.sleep(1.1)
    gen, data = row.path("gen/src/lib.rs"), row.path("core/data.txt")
    in_place(gen, open(gen).read().replace("const SALT: u64 = 7;", "const SALT: u64 = 5;"))
    in_place(data, re.sub(r"^alpha \d+", "alpha 2", open(data).read(), flags=re.M))
    row.git("commit", "-qam", "another salt and other data")
    row.warm()
    row.both("the first, with a salt and data of its own", want="Reading/5")
    row.both("the worktree asked second", ws=tree, want="Reading/7")
    row.warm(ws=tree)
    for ws, salt in ((tree, 7), (row.ws, 5), (tree, 7)):
        row.edit(os.path.join(ws, "core/src/lib.rs"))
        how, _ = row.both(f"salt {salt}, edited", ws=ws, want=f"Reading/{salt}")
        row.warm(ws=ws)


def non_utf8_names(row):
    src = row.path("core/src").encode()
    with open(os.path.join(src, b"caf\xe9.rs"), "wb") as file:
        file.write(b"pub fn unused() {}\n")
    with open(os.path.join(src, b"latin.rs"), "wb") as file:
        file.write(b"// caf\xe9 is not UTF-8\npub fn unused() {}\n")
    os.makedirs(os.path.join(src, b"d\xff"))
    with open(os.path.join(src, b"d\xff", b"mod.rs"), "wb") as file:
        file.write(b"pub fn unused() {}\n")
    row.warm()
    row.both("with files cargo does not compile: a name and a text that are not UTF-8")
    row.edit()
    row.both("edited", want=f"mark {row.mark}")
    with open(os.path.join(src, b"latin.rs"), "ab") as file:
        file.write(b"// \xff\n")
    row.edit()
    row.both("the text that is not UTF-8 changed", want=f"mark {row.mark}")
    row.both("tests", command="test")


def non_utf8_root(row):
    """The whole workspace under a directory whose name is not UTF-8."""
    under = os.path.join(row.dir.encode(), b"r\xff\xfe")
    os.makedirs(under)
    ws = os.path.join(under, b"ws")
    shutil.copytree(row.ws.encode(), ws, ignore=lambda _, names: [it for it in names if it.startswith(b"target")])
    run = lambda argv, env: subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True, errors="replace", timeout=1800)
    for label in ("cold", "edited"):
        ours = run(["blaze", "run"], row.env)
        _, fresh = Scratch(os.fsdecode(ws), row.env).cargo("run", timeout=1800)
        shutil.rmtree(os.path.join(ws, b"target-scratch"), ignore_errors=True)
        how = how_built(ours.stderr)
        row.built.append(f"{label}: {how}")
        if (ours.returncode, ours.stdout) != (fresh.returncode, fresh.stdout):
            raise Wrong(f"{label} ({how}): {ours.returncode} {ours.stdout[-200:]!r} {ours.stderr[-300:]!r}, and from nothing {fresh.returncode} {fresh.stdout[-200:]!r}")
        if ours.returncode != 0 or f"mark {row.mark}" not in ours.stdout:
            raise Wrong(f"{label} ({how}): exits {ours.returncode} without `mark {row.mark}`: {ours.stdout[-200:]!r} {ours.stderr[-300:]!r}")
        lib = os.path.join(ws, b"core/src/lib.rs")
        in_place(lib, MARK.sub(row.next_mark(), open(lib).read()))
        time.sleep(5)


def other_filesystem(row):
    here, there = os.stat(row.ws).st_dev, os.stat(row.store if os.path.isdir(row.store) else row.dir).st_dev
    if here == there:
        raise Wrong("the workspace and the store are on one filesystem here; name another with --other-fs")
    row.warm()
    for round in range(3):
        row.edit()
        row.both(f"edited, {round}", want=f"mark {row.mark}")
    row.both("tests", command="test")
    packs, files = (glob.glob(os.path.join(row.store, kind)) for kind in ("pack-*", "file-*"))
    stored = sum(os.path.getsize(it) for it in packs + files)
    row.notes.append(f"{stored} bytes in {len(packs)} packs and {len(files)} files in the store on the other filesystem")
    if not stored:
        raise Wrong("nothing was stored")


def says_its_rustc(row):
    """Has the program say which rustc built it."""
    build = open(row.path("core/build.rs")).read()
    said = ('    let rustc = std::env::var("RUSTC").unwrap();\n    let version = std::process::Command::new(rustc).arg("--version").output().unwrap();\n'
            '    println!("cargo:rustc-env=SOAK_RUSTC={}", String::from_utf8(version.stdout).unwrap().trim());\n')
    in_place(row.path("core/build.rs"), build.replace("fn main() {", "fn main() {\n" + said, 1))
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib + '\npub fn rustc() -> &\'static str { env!("SOAK_RUSTC") }\n')
    main = open(row.path("app/src/main.rs")).read()
    in_place(row.path("app/src/main.rs"), main.replace("fn main() {", 'fn main() {\n    println!("{}", soak_core::rustc());', 1))


def toolchain_switched(row):
    """The toolchain file of the directory changes under a live daemon, as `rustup
    override` or a checkout does. `blaze` builds with the release it compiles whatever
    the file says (#269), from nothing and on the daemon alike."""
    says_its_rustc(row)
    file = row.path("rust-toolchain.toml")
    ours = lambda version: in_place(file, f'[toolchain]\nchannel = "{version}"\n')
    ours(row.env["RUSTUP_TOOLCHAIN"])
    row.env.pop("RUSTUP_TOOLCHAIN")
    row.warm()
    row.both("the toolchain blaze is built for", want=f"rustc {TOOLCHAIN}")
    ours(OTHER_TOOLCHAIN)
    row.both(f"the file now says {OTHER_TOOLCHAIN}", want=f"rustc {TOOLCHAIN}")
    row.edit()
    how, _ = row.both(f"edited under {OTHER_TOOLCHAIN}", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"an edit under a file naming {OTHER_TOOLCHAIN} was not the daemon's: {how}")
    ours(TOOLCHAIN)
    row.edit()
    row.both("the file says the first again", want=f"rustc {TOOLCHAIN}")
    row.edit()
    row.both("edited after that", want=f"mark {row.mark}")


def toolchains_in_turn(row):
    """Two toolchains named by the environment of the client, turn by turn: `blaze`
    builds with the release it compiles under both (#269)."""
    says_its_rustc(row)
    row.warm()
    for round in range(3):
        for version in (OTHER_TOOLCHAIN, TOOLCHAIN):
            row.edit()
            how, ran = row.both(f"{version}, {round}", env={"RUSTUP_TOOLCHAIN": version}, want=f"rustc {TOOLCHAIN}")
            if f"mark {row.mark}" not in ran.stdout:
                raise Wrong(f"{version}, {round}: not the edit")
            if how != "daemon":
                raise Wrong(f"{version}, {round}: not the daemon's: {how}")


def pinned_to_a_nightly(row):
    """A project pinned to a nightly of the next release, built with blaze's own Rust
    (the pin is ignored), as bun is: nightly cargo passes every crate
    `-Z embed-metadata=no` and writes its build directory in its second layout, where the
    build script's output is. Each sent the build to cargo."""
    if shutil.which("rustup"):
        subprocess.run(["rustup", "toolchain", "install", NIGHTLY, "--profile", "minimal"],
                       check=True, capture_output=True)
    says_its_rustc(row)
    row.env.update(RUSTUP_TOOLCHAIN=NIGHTLY)
    row.warm()
    row.both("warm", want="-nightly")
    row.edit()
    how, _ = row.both("a body edited", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the build after a body was edited was not the daemon's: {how}")


# The programs of one build, which run each other from next to themselves: with one missing,
# `blaze` is cargo's.
PROGRAMS = ("blaze-engine", "blaze")


def another_build(source, bin):
    """A blaze that is another build: the same program with bytes after its end."""
    for name in PROGRAMS:
        other = os.path.join(bin, name + ".new")
        shutil.copy(os.path.join(source, name), other)
        with open(other, "ab") as file:
            file.write(b"\0" * 16)
        os.replace(other, os.path.join(bin, name))


def binary_replaced(row):
    source, bin, old = os.path.dirname(BLAZE), os.path.join(row.dir, "bin"), os.path.join(row.dir, "bin-old")
    for dir in (bin, old):
        os.makedirs(dir)
        for name in PROGRAMS:
            shutil.copy2(os.path.join(source, name), os.path.join(dir, name))
    path = row.env["PATH"]
    row.env["PATH"] = bin + os.pathsep + path
    row.warm()
    row.edit()
    row.both("before", want=f"mark {row.mark}")
    first = row.status()[0]

    another_build(source, bin)
    row.edit()
    row.both("the binary replaced under the daemon", want=f"mark {row.mark}")
    second = row.status()[0]
    if second in (None, first) or os.path.exists(f"/proc/{first}/exe") and "Z" not in open(f"/proc/{first}/stat").read().split(")")[-1].split()[0]:
        raise Wrong(f"the daemon of the build that is gone is still there: {first} then {second}")
    row.notes.append(f"daemon {first} was replaced by {second}")
    wait_warm(Side(row.ws, row.env, True), row.ws)
    row.edit()
    how, _ = row.both("the build after, by the new daemon", want=f"mark {row.mark}")

    # A client of the build before meets the daemon of the build after.
    older = {"PATH": old + os.pathsep + path}
    row.edit()
    row.both("an older client and a newer daemon", env=older, want=f"mark {row.mark}")
    third = row.status(older)[0]
    row.notes.append(f"the older client replaced daemon {second} by {third}")
    row.edit()
    row.both("and the newer client again", want=f"mark {row.mark}")
    wait_warm(Side(row.ws, row.env, True), row.ws)

    # The file the daemon runs from is deleted, and it is asked for a workspace it has no
    # worker for, by a client that is the same build from another place.
    twin = os.path.join(row.dir, "bin-twin")
    shutil.copytree(bin, twin, copy_function=shutil.copy2)
    daemon = row.status()[0]
    os.remove(os.path.join(bin, "blaze-engine"))
    second_ws = os.path.join(row.dir, "ws2")
    row.copy(second_ws)
    apart = {"PATH": twin + os.pathsep + path}
    row.both("the binary deleted, another workspace", ws=second_ws, env=apart, want="mark 1000")
    # A daemon cannot start a worker from a file that is gone: it ends, and the next build
    # starts the one that is there.
    time.sleep(3)
    row.both("the build after that", ws=second_ws, env=apart, want="mark 1000")
    after = row.status(apart)[0]
    if after in (None, daemon):
        raise Wrong(f"the daemon whose binary was deleted is still the one asked: {daemon} then {after}")
    wait_warm(Side(second_ws, dict(row.env, **apart), True), second_ws)
    lib = os.path.join(second_ws, "core/src/lib.rs")
    row.edit(lib)
    how, _ = row.both("edited there", ws=second_ws, env=apart, want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the daemon that replaced the one whose binary was deleted did not build the edit: {how}")
    row.notes.append(f"daemon {daemon} whose binary was deleted was replaced by {after}")
    row.stop(apart)


def memory_ceiling(row):
    """With a ceiling below what one worker holds, the worker asked longest ago is stopped,
    and its workspace builds right after."""
    second = os.path.join(row.dir, "ws2")
    row.copy(second)
    row.warm()
    row.edit()
    how, _ = row.both("the first workspace, loaded", want=f"mark {row.mark}")
    _, first_workers, text = row.status()
    row.notes.append("one workspace loaded:\n" + text)
    row.warm(ws=second)
    time.sleep(3)
    _, workers, text = row.status()
    row.notes.append("a second loaded, three seconds later:\n" + text)
    if any(worker in workers for worker in first_workers):
        raise Wrong(f"the worker asked longest ago is still there: {first_workers} of {workers}")
    if not workers:
        raise Wrong("the worker asked last was stopped too")
    row.edit()
    row.both("the first workspace, its worker stopped", want=f"mark {row.mark}")
    wait_warm(Side(row.ws, row.env, True), row.ws)
    row.edit()
    how, _ = row.both("the first workspace, loaded again", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"it was not loaded again: {how}")
    row.both("its tests", command="test")
    time.sleep(3)
    row.notes.append("after the first was asked again:\n" + row.status()[2])
    log = open(os.path.join(row.home, "daemon.log"), errors="replace").read()
    row.notes.append("\n".join(l for l in log.splitlines() if "BLAZE_MEMORY_MAX" in l))


def no_ceiling(row):
    """The same two workspaces with no ceiling: both workers stay."""
    second = os.path.join(row.dir, "ws2")
    row.copy(second)
    row.warm()
    row.warm(ws=second)
    time.sleep(3)
    _, workers, text = row.status()
    row.notes.append(text)
    if len(workers) != 2:
        raise Wrong(f"{len(workers)} workers for two workspaces")
    row.edit()
    how, _ = row.both("the first workspace", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"not the daemon's: {how}")


def in_a_control_group(row, allowed=6 << 30):
    """A daemon started in a control group that allows 6 GB, as a container, a CI job or a
    systemd unit is, has its workers hold at most half of that, not half of the machine's
    memory, and builds."""
    scope = ["systemd-run", "--user", "--scope", "--quiet", "-p", f"MemoryMax={allowed}"]
    if not sys.platform.startswith("linux") or not shutil.which("systemd-run"):
        raise NotRun("it starts a control group with systemd-run, on Linux")
    probe = subprocess.run([*scope, "sh", "-c", 'cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'],
                           capture_output=True, text=True)
    if probe.stdout.strip() != str(allowed):
        raise NotRun(f"no user systemd that limits a scope's memory: {(probe.stdout + probe.stderr).strip()[-200:]}")
    side = Side(row.ws, row.env, True)
    ran = subprocess.run([*scope, *side.argv("build")], cwd=row.ws, env=side.env, capture_output=True, text=True, timeout=1800)
    if ran.returncode:
        raise Wrong(f"the first build failed: {ran.stderr[-300:]}")
    wait_warm(side, row.ws)
    text = row.status()[2]
    row.notes.append(text)
    most = re.search(r"workers hold at most (\d+) MB", text)
    if not most or int(most[1]) > allowed // 2 >> 20:
        raise Wrong(f"the workers may hold more than half of the {allowed >> 20} MB the control group allows: {text.strip()[-300:]}")
    row.edit()
    how, _ = row.both("a body edited", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the build after a body was edited was not the daemon's: {how}")


def generated_for_a_proc_macro(row):
    """A proc macro that `include!`s what its build script wrote to `OUT_DIR`, as biome's
    macros are built on `biome_aria_metadata`: the script writes it while cargo builds, after
    the build began, and the first edit after the build is still the daemon's."""
    open(row.path("gen/build.rs"), "w").write(
        'fn main() {\n    println!("cargo:rerun-if-changed=build.rs");\n'
        '    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("pepper.rs");\n'
        '    std::fs::write(out, "#[allow(dead_code)]\\nconst PEPPER: u64 = 3;\\n").unwrap();\n}\n')
    with open(row.path("gen/src/lib.rs"), "a") as lib:
        lib.write('\ninclude!(concat!(env!("OUT_DIR"), "/pepper.rs"));\n')
    row.git("add", "."), row.git("commit", "-qm", "a build script for the proc macro")
    row.warm()
    row.edit()
    how, _ = row.both("a body edited after the first build", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the first edit after the build was not the daemon's: {how}")


def script_touches_its_source(row):
    """A proc macro whose build script makes a file of its crate's newer, as biome's analyzer
    crates touch `src/lint/nursery.rs` for cargo to build the crate again: the script runs
    after the build began and before rustc reads the file, which is no change to cargo, and
    the first edit after the build is still the daemon's."""
    open(row.path("gen/build.rs"), "w").write(
        'fn main() {\n    println!("cargo:rerun-if-changed=build.rs");\n'
        '    let lib = std::fs::File::options().write(true).open("src/lib.rs").unwrap();\n'
        '    lib.set_modified(std::time::SystemTime::now()).unwrap();\n}\n')
    row.git("add", "."), row.git("commit", "-qm", "a build script that touches the proc macro")
    row.warm()
    row.edit()
    how, _ = row.both("a body edited after the first build", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the first edit after the build was not the daemon's: {how}")


def proc_macro_tests(row):
    """The tests of every package, the proc macro's own among them, which link the
    toolchain's `libstd`."""
    with open(row.path("gen/src/lib.rs"), "a") as lib:
        lib.write("\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn salt_is_seven() {\n        assert_eq!(super::SALT, 7);\n    }\n}\n")
    row.both("cold", command="test", args=("--workspace",))
    row.warm()
    row.edit()
    row.both("warm", command="test", args=("--workspace",))
    row.both("the proc macro alone", command="test", args=("-p", "soak-gen"))


def another_under_test(row):
    """A function that is one thing in the program and another in its tests, where it reads
    a thread local a test has set: built and tested in turn, with the profile of the package
    changed between."""
    main = row.path("app/src/main.rs")
    text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("now {} {}", now(), mark());', 1)
    text += """
#[cfg(not(test))]
fn now() -> String {
    "one".to_owned()
}

#[cfg(test)]
thread_local! {
    static SET: std::cell::RefCell<Option<String>> = None.into();
}

#[cfg(test)]
fn now() -> String {
    SET.with_borrow(|set| set.as_ref().cloned().unwrap_or_else(|| "two".to_owned()))
}

pub fn mark() -> u64 { 0 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_what_was_set() {
        assert_eq!(now(), "two");
        SET.with_borrow_mut(|set| *set = Some("three".to_owned()));
        assert_eq!(now(), "three");
        assert!(mark() < 100);
    }
}
"""
    open(main, "w").write(text)
    row.both("cold", want="now one")
    row.both("cold tests", command="test")
    row.warm()
    for turn in range(3):
        row.edit(main)
        row.both("edited", want="now one")
        how, _ = row.both("edited, tests", command="test")
        if how != "daemon":
            raise Wrong(f"the tests were not built by the daemon: {how}")
        if turn == 0:
            with open(row.path("Cargo.toml"), "a") as manifest:
                manifest.write("\n[profile.dev.package.soak-app]\ndebug = false\n")
            row.both("another profile, tests", command="test")
            row.warm()


def kept_to_an_object(row):
    """Tests built without incremental compilation, where rustc keeps to one object the
    functions nothing outside it calls: a test sets a thread local that a function of another
    module reads, and then a test changes into one that fails."""
    lib = row.path("core/src/lib.rs")
    with open(lib, "a") as text:
        text.write("""
pub mod clock {
    #[cfg(test)]
    thread_local! {
        pub(crate) static SET: std::cell::RefCell<Option<u64>> = None.into();
    }

    #[cfg(not(test))]
    fn now() -> u64 {
        7
    }

    #[cfg(test)]
    fn now() -> u64 {
        SET.with_borrow(|set| set.unwrap_or(7))
    }

    pub fn after(then: u64) -> bool {
        now() > then
    }
}

#[cfg(test)]
mod clock_tests {
    use super::clock::{SET, after};

    struct Set;

    impl Set {
        fn new(now: u64) -> Set {
            SET.with_borrow_mut(|set| *set = Some(now));
            Set
        }
    }

    impl Drop for Set {
        fn drop(&mut self) {
            SET.with_borrow_mut(|set| *set = None);
        }
    }

    #[test]
    fn after_what_was_set() {
        let _set = Set::new(100);
        assert!(after(50));
        assert!(super::mark() > 0);
    }

    #[test]
    fn sums() {
        assert_eq!(1 + 1, 2);
    }
}
""")
    args = ("-p", "soak-core")
    row.both("cold tests", command="test", args=args)
    row.warm()
    row.edit()
    row.both("edited, tests", command="test", args=args)
    row.warm()
    in_place(lib, open(lib).read().replace("assert_eq!(1 + 1, 2);", "assert_eq!(1 + 1, 3);"))
    _, ours = row.both("a test that now fails", command="test", args=args, fails=True)
    if "sums ... FAILED" not in ours.stdout:
        raise Wrong(f"the test that fails was not seen to: {ours.stdout[-300:]}")
    row.edit()
    row.both("edited again, tests", command="test", args=args, fails=True)


def show_in_main(row, shown):
    main = row.path("app/src/main.rs")
    in_place(main, open(main).read().replace("fn main() {\n", "fn main() {\n" + shown, 1))


def beside_one_left(row):
    """A function left to rustc from the first build, `seven` (for the constant
    `breakers.py caller_in_constant` is about): an item added to another file of its crate
    could change what rustc's copy of it would be, and the build failed ("has an error blaze
    cannot say more of"). It and a body edited after it are built, each by the daemon or by
    cargo: which depends on whether rustc's copy is from before the crate's last edit. `seven`
    is said to be left to rustc once an edit has compiled its crate: a daemon that starts from
    rustc's build compiles nothing at first."""
    in_place(row.path("core/src/later.rs"), "pub fn eight() -> u64 {\n    8\n}\n")
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write("""
pub fn seven() -> u64 {
    const CALLER: &std::panic::Location<'static> = std::panic::Location::caller();
    6 + u64::from(CALLER.line() > 0)
}

pub mod later;
""")
    show_in_main(row, '    println!("seven {} eight {}", soak_core::seven(), soak_core::later::eight());\n')
    row.both("cold", want="seven 7 eight 8")
    row.warm()
    row.both("warm", want="seven 7 eight 8")
    later = row.path("core/src/later.rs")
    in_place(later, open(later).read() + "\npub fn nine() -> u64 {\n    9\n}\n")
    how, _ = row.both("an item added to another file of the crate", want="seven 7 eight 8")
    if how != "daemon" and not how.startswith("cargo"):
        raise Wrong(f"the build after an item was added was neither the daemon's nor cargo's: {how}")
    row.warm()
    in_place(later, open(later).read().replace("    8\n", "    80\n"))
    how, _ = row.both("a body edited in that file", want="seven 7 eight 80")
    if how != "daemon" and not how.startswith("cargo"):
        raise Wrong(f"the build after a body was edited was neither the daemon's nor cargo's: {how}")
    logs = glob.glob(os.path.join(row.home, "**", "log"), recursive=True)
    said = "".join(open(log, errors="replace").read() for log in logs)
    if not re.search(r"left to rustc: soak_core::seven: ", said):
        raise Wrong("`seven` is not left to rustc: the row needs another function that is")


def through_a_blanket_impl(row):
    """`layout::<Well>` reaches `Well::render` through `impl<T: RenderOnce> View for T`, both
    returning `impl Value` (#25). blaze could not lay out the type, left `layout::<Well>` and
    `main` to rustc without a word, and rustc's copies call rustc's `Well::render`: every edit
    of it was "applied: 0 functions", and the program on disk ran the code from before."""
    with open(row.path("app/src/main.rs"), "a") as main:
        main.write("""
trait Value {
    fn get(self) -> u32;
}

impl Value for u32 {
    fn get(self) -> u32 {
        self
    }
}

trait RenderOnce {
    fn render(self) -> impl Value;
}

trait View {
    fn render(self) -> impl Value;
}

impl<T: RenderOnce> View for T {
    fn render(self) -> impl Value {
        RenderOnce::render(self)
    }
}

struct Well;

impl RenderOnce for Well {
    fn render(self) -> impl Value {
        1
    }
}

fn layout<V: View>(view: V) -> u32 {
    view.render().get()
}
""")
    show_in_main(row, '    println!("well {}", layout(Well));\n')
    row.both("cold", want="well 1")
    row.warm()
    main = row.path("app/src/main.rs")
    for before, after in [("1", "2"), ("2", "3")]:
        in_place(main, open(main).read().replace(f"        {before}\n", f"        {after}\n"))
        how, _ = row.both(f"`Well::render` edited to {after}", want=f"well {after}")
        if how != "daemon":
            raise Wrong(f"the build of `Well::render` edited to {after} was not the daemon's: {how}")


def reached_from_one_left(row):
    """What rustc's copy of a function left to rustc calls is rustc's copy too where blaze's
    can't take its place: a function private to rustc's object (`offset`, which only `seven`
    calls) and a generic one (`scaled::<u8>`). An edit of either, or of `seven` before the
    daemon had a build of its own, was not in the program the daemon built. Each is built by
    the daemon or by cargo, and runs as its source says. An item added to `main` first
    has the daemon build in full, with `seven` left to rustc as rustc compiled it."""
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write("""
pub fn seven() -> u64 {
    const CALLER: &std::panic::Location<'static> = std::panic::Location::caller();
    6 + u64::from(CALLER.line() > 0) + helpers::offset() + helpers::scaled(3u8)
}

mod helpers;
""")
    in_place(row.path("core/src/helpers.rs"), """pub(crate) fn offset() -> u64 {
    0
}

pub(crate) fn scaled<T: Into<u64>>(x: T) -> u64 {
    x.into() * 0
}
""")
    show_in_main(row, '    println!("seven {}", soak_core::seven());\n')
    row.both("cold", want="seven 7")
    lib, helpers = row.path("core/src/lib.rs"), row.path("core/src/helpers.rs")
    main = row.path("app/src/main.rs")
    for label, file, before, after, want in [
        ("`seven` edited", lib, "    6 + u64", "    16 + u64", "seven 17"),
        ("an item added to `main`'s file", main, "fn main() {", "fn unused() {}\n\nfn main() {",
         "seven 17"),
        ("`offset`, which only `seven` calls, edited", helpers, "    0\n", "    100\n",
         "seven 117"),
        ("`scaled`, a generic `seven` calls, edited", helpers, "x.into() * 0", "x.into() * 1000",
         "seven 3117"),
    ]:
        if not label.startswith("`offset`"):
            row.warm()
        in_place(file, open(file).read().replace(before, after, 1))
        how, _ = row.both(label, want=want)
        if how != "daemon" and not how.startswith("cargo"):
            raise Wrong(f"{label}: the build was neither the daemon's nor cargo's: {how}")
    logs = glob.glob(os.path.join(row.home, "**", "log"), recursive=True)
    said = "".join(open(log, errors="replace").read() for log in logs)
    if not re.search(r"left to rustc: soak_core::seven: ", said):
        raise Wrong("`seven` is not left to rustc: the row needs another function that is")


def forwarded_edited(row):
    """An instance that does not compile, `located::<u8>` (for the constant `breakers.py
    caller_in_constant` is about): `main` calls rustc's copy of it. An edit of the generic
    function before the daemon first built in full kept that copy, and the program printed
    what it did before the edit."""
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write("""
pub fn located<T: Default + Into<u64>>() -> u64 {
    const CALLER: &std::panic::Location<'static> = std::panic::Location::caller();
    T::default().into() + u64::from(CALLER.line() > 0)
}
""")
    show_in_main(row, '    println!("located {}", soak_core::located::<u8>());\n')
    row.both("cold", want="located 1")
    row.warm()
    row.both("warm", want="located 1")
    lib = row.path("core/src/lib.rs")
    in_place(lib, open(lib).read().replace("into() + u64::from", "into() + 10 * u64::from"))
    how, _ = row.both("the generic function edited", want="located 10")
    if how != "daemon" and not how.startswith("cargo"):
        raise Wrong(f"the edit was neither the daemon's nor cargo's: {how}")


def statics_one_unit_uses(row):
    """Statics written by a function left to rustc (`left`, for the constant `breakers.py
    caller_in_constant` is about) that only the functions of one codegen unit use, as bun's
    `SHOULD_ABORT` and the `FLAG` of a method of `impl<const SSL: bool, ..> NewServer<SSL, ..>`
    are: rustc makes them local to the unit. blaze can't walk `left`, so rustc's copy of it
    may reach any static of its crate, also `FLAG` through rustc's copy of `flag`. The base the
    daemon builds of its own after the first patch can't link to a local symbol: it took the
    `FLAG` in a body as its own, apart from the one rustc's `left` counts with, and a patch of
    that base printed what the source does not say. The edits are of `main`'s crate: one of `left`'s
    could change what rustc's copy of it would be, and goes to cargo."""
    in_place(row.path("core/src/gate.rs"), """use std::sync::atomic::{AtomicU64, Ordering};

pub struct Gate<const LOUD: bool>;

impl<const LOUD: bool> Gate<LOUD> {
    pub fn flag() -> &'static AtomicU64 {
        static FLAG: AtomicU64 = AtomicU64::new(0);
        &FLAG
    }
}

pub fn left() -> u64 {
    const CALLER: &std::panic::Location<'static> = std::panic::Location::caller();
    Gate::<true>::flag().fetch_add(1, Ordering::Relaxed) + u64::from(CALLER.line() == 0)
}
""")
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write("""
pub mod gate;

pub fn shown() -> u64 {
    gate::Gate::<true>::flag().fetch_add(1, std::sync::atomic::Ordering::Relaxed) * 10
}
""")
    show_in_main(row, '    println!("left {} shown {} left {}", soak_core::gate::left(), soak_core::shown(), '
                      'soak_core::gate::left());\n')
    row.both("cold", want="left 0 shown 10 left 2")
    row.warm()
    row.both("warm", want="left 0 shown 10 left 2")
    main = row.path("app/src/main.rs")
    in_place(main, open(main).read().replace('"left {} shown', '"Left {} shown', 1))
    row.both("main edited", want="Left 0 shown 10 left 2")
    logs = lambda: "".join(open(it, errors="replace").read()
                           for it in glob.glob(os.path.join(row.home, "**", "log"), recursive=True))
    began = time.time()
    while "compiling the program for edits to patch" not in logs() and time.time() - began < 120:
        time.sleep(0.5)
    based = logs().split("compiling the program for edits to patch", 1)
    if len(based) < 2:
        raise Wrong("the daemon built no base of its own after the first patch")
    while "stats:" not in logs().split("compiling the program for edits to patch", 1)[1]:
        if time.time() - began > 300:
            raise Wrong("the daemon's own base did not finish")
        time.sleep(0.5)
    in_place(main, open(main).read().replace('"Left {} shown', '"LEFT {} shown', 1))
    how, _ = row.both("main edited again, over the daemon's base", want="LEFT 0 shown 10 left 2")
    if how != "daemon" and not how.startswith("cargo"):
        raise Wrong(f"the edit over the daemon's base was neither the daemon's nor cargo's: {how}")
    if not re.search(r"left to rustc: soak_core::gate::left: ", logs()):
        raise Wrong("`left` is not left to rustc: the row needs another function that is")


def outside_its_package(row):
    """A module whose file is outside its package (`#[path = "../../shared/outside.rs"]`), as
    bun's runtime has: the file is in no package's directory, so it was not loaded, the
    function that calls into it was left to rustc, and an edit to the file was not seen: the
    program the daemon built ran it as it was."""
    os.makedirs(row.path("shared"), exist_ok=True)
    outside = row.path("shared/outside.rs")
    in_place(outside, "pub fn seven() -> u64 {\n    7\n}\n")
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write('\n#[path = "../../shared/outside.rs"]\nmod outside;\n\npub fn seven() -> u64 {\n    outside::seven()\n}\n')
    show_in_main(row, '    println!("seven {}", soak_core::seven());\n')
    row.both("cold", want="seven 7")
    row.warm()
    row.both("warm", want="seven 7")
    in_place(outside, open(outside).read().replace("    7\n", "    70\n"))
    how, _ = row.both("a body edited in the file", want="seven 70")
    if how != "daemon":
        raise Wrong(f"the build after a body was edited was not the daemon's: {how}")
    in_place(outside, open(outside).read() + "\npub fn eight() -> u64 {\n    8\n}\n")
    in_place(outside, open(outside).read().replace("    70\n", "    eight() * 10\n"))
    row.both("an item added there and called", want="seven 80")


ROWS = {
    "symlinked file": (symlinked_file, {}),
    "symlinked directory": (symlinked_directory, {}),
    "symlinked workspace root": (symlinked_root, {}),
    "save by rename": (save_by_rename, {}),
    "save by truncate and write": (save_by_truncate, {}),
    "changed twice within the time stamp": (twice_in_a_tick, {}),
    "checkout of thousands of files": (checkout_of_thousands, {}),
    "case-only rename": (case_only_rename, {}),
    "worktree deleted and recreated": (worktree_recreated, {}),
    "worktrees whose libraries were built apart": (worktrees_built_apart, {}),
    "worktrees built by other code": (worktrees_that_run_other_code, {}),
    "names and text that are not UTF-8": (non_utf8_names, {}),
    "workspace under a name that is not UTF-8": (non_utf8_root, {}),
    "store on another filesystem": (other_filesystem, {}),
    "toolchain file switched": (toolchain_switched, {}),
    "two toolchains in turn": (toolchains_in_turn, {}),
    "daemon binary replaced": (binary_replaced, {}),
    "memory ceiling": (memory_ceiling, {"BLAZE_MEMORY_MAX": "1M"}),
    "no memory ceiling": (no_ceiling, {"BLAZE_MEMORY_MAX": "off"}),
    "control group's memory": (in_a_control_group, {}),
    "tests of a proc macro": (proc_macro_tests, {}),
    "proc macro on generated code": (generated_for_a_proc_macro, {}),
    "proc macro its script touches": (script_touches_its_source, {}),
    "a function that is another under test": (another_under_test, {}),
    "functions rustc keeps to an object": (kept_to_an_object, {"CARGO_INCREMENTAL": "0"}),
    "an item added beside a function left to rustc": (beside_one_left, {}),
    "an instance rustc's copy stands in for, edited": (forwarded_edited, {}),
    "statics one codegen unit uses": (statics_one_unit_uses, {}),
    "a module file outside its package": (outside_its_package, {}),
    "an edit reached through a blanket impl": (through_a_blanket_impl, {}),
    "an edit only a function left to rustc reaches": (reached_from_one_left, {}),
    "a project pinned to a nightly": (pinned_to_a_nightly, {}),
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("rows", nargs="*")
    parser.add_argument("--work")
    parser.add_argument("--other-fs", default="/dev/shm")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    if not shutil.which("blaze", path=os.path.dirname(BLAZE)):
        sys.exit("no blaze beside the blaze of BLAZE_BIN")
    unknown = [wanted for wanted in args.rows if not any(wanted in name for name in ROWS)]
    if unknown:
        sys.exit(f"no row is named like {unknown}; the rows are: {', '.join(ROWS)}")
    work = args.work or tempfile.mkdtemp(prefix="blaze-soakrows-")
    table, failed = [], 0
    for name, (run, env) in ROWS.items():
        if args.rows and not any(wanted in name for wanted in args.rows):
            continue
        slug = re.sub(r"\W+", "-", name)
        at = None
        if run is other_filesystem:
            at = os.path.join(tempfile.mkdtemp(prefix="blaze-soakrows-", dir=args.other_fs), "ws")
        row, began = Row(work, slug, env, at), time.time()
        try:
            run(row)
            found = row.panics()
            if found:
                raise Wrong(f"{len(found)} panics logged: {found[0][:300]}")
            result = "right"
        except NotRun as why:
            result = f"not run: {why}"
        except Wrong as wrong:
            result, failed = f"WRONG: {wrong}", failed + 1
        except (subprocess.SubprocessError, OSError) as error:
            result, failed = f"WRONG: the row could not go on: {error}", failed + 1
        finally:
            row.stop()
            if at:
                shutil.rmtree(os.path.dirname(at), ignore_errors=True)
        table.append((name, result, row.built, row.notes))
        print(f"{'ok ' if result == 'right' else '-- ' if result.startswith('not run') else 'NOT'} {name}  {time.time() - began:.0f}s  {result if result != 'right' else ''}", flush=True)
        for line in row.built:
            print(f"      {line}", flush=True)
    with open(os.path.join(HERE, "SOAKROWS.md"), "w") as out:
        out.write(f"# soakrows.py\n\n{len(table) - failed} of {len(table)} rows right.\n\n| row | result | who built each comparison |\n|---|---|---|\n")
        for name, result, built, _ in table:
            out.write(f"| {name} | {result.replace('|', '/')[:400]} | {'; '.join(built).replace('|', '/')} |\n")
        for name, _, _, notes in table:
            if notes:
                out.write(f"\n## {name}\n\n```\n" + "\n".join(notes) + "\n```\n")
    print(f"{len(table) - failed} of {len(table)} rows right")
    if not args.keep and not failed and not args.work:
        shutil.rmtree(work, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
