"""clone_out.py: a second checkout of a project costs only its own bytes
(docs/v2/plan/design/clone-out.md).

    uv run --frozen python harness/v2/clone_out.py '{"engine": {"kind": "v2", "blaze": "<blaze>"}, "out": "/tmp/clone-out.json"}'
    uv run --frozen python harness/v2/clone_out.py '{"engine": {...}, "out": "...", "work": "/mnt/btrfs/clone-out"}'
    uv run --frozen python harness/v2/clone_out.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine              the blaze measured (engine.EngineSpec)
    out                 the result's path (report.py's JSON)
    work                where the checkouts and their one home go: a directory on a volume
                        that clones (APFS; btrfs or XFS with reflink). A new directory under
                        /tmp when empty
    fixture_seed        the oracle's fixture state built: "base", or any other seed for a random one
    most_added_share    the bytes the second checkout may add, as a share of its build
                        directory's bytes (0.10)
    most_added_bytes    ... or this many, whichever is more (4 MiB): a small project's few
                        fresh files (OUT_DIRs, run files, outputs under 16 KiB) are more than a
                        share of it
    timeout_s           each build's bound (3600)
    keep                keep the work directory (false)

One check, `a_second_checkout_adds_only_its_own_bytes`. The fixture is copied into two checkouts,
`a/ws` and `b/ws`, which are built one after the other by one daemon (one `BLAZE_HOME`, so one
store). What the second checkout added to the disk is then:

    private   the bytes of `b`'s build directory shared with no other file (macOS:
              `getattrlist`'s `ATTR_CMNEXT_PRIVATESIZE`; Linux: `FIEMAP` extents without
              `FIEMAP_EXTENT_SHARED`)
    placed    the bytes of the store's `placed/` files made since the first build (shared with
              `b`'s clones of them, so in no file's private bytes)
    store     the store's other files' growth since the first build

The check passes when their sum is at most the larger of the two limits. It reports the `placed`
counts of both builds' `blaze-work` lines (cloned, written, copied). A volume that can't clone
fails both builds with blaze's error, which names the volume. `selftest` clones a file on
`work`'s volume and checks that the private-bytes measure sees the clone share its blocks, with
no blaze.
"""

import asyncio
import ctypes
import os
import shutil
import struct
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import engine
import fixture
import report
from typed_input import fail, parse

SCRIPT = "clone_out.py"
CHECK = "a_second_checkout_adds_only_its_own_bytes"


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    fixture_seed: str = "base"
    most_added_share: float = 0.10
    most_added_bytes: int = 4 << 20
    timeout_s: float = 3600
    keep: bool = False


@dataclass(frozen=True)
class Tree:
    """A directory's regular files: their bytes, and the bytes shared with no other file."""

    files: int
    size: int
    private: int


# macOS: getattrlist(2).
ATTR_CMN_RETURNED_ATTRS = 0x8000_0000
ATTR_CMNEXT_PRIVATESIZE = 0x0000_0008
FSOPT_NOFOLLOW = 0x1
FSOPT_ATTR_CMN_EXTENDED = 0x20


class AttrList(ctypes.Structure):
    _fields_ = [("bitmapcount", ctypes.c_ushort), ("reserved", ctypes.c_uint16), ("commonattr", ctypes.c_uint32),
                ("volattr", ctypes.c_uint32), ("dirattr", ctypes.c_uint32), ("fileattr", ctypes.c_uint32),
                ("forkattr", ctypes.c_uint32)]


# Linux: linux/fiemap.h and linux/fs.h.
FS_IOC_FIEMAP = 0xC020_660B
FIEMAP_FLAG_SYNC = 0x1
FIEMAP_EXTENT_LAST = 0x1
FIEMAP_EXTENT_SHARED = 0x2000
FIEMAP_HEAD = struct.Struct("=QQIIII")
FIEMAP_EXTENT = struct.Struct("=QQQQQIIII")
EXTENTS_PER_CALL = 256
FICLONE = 0x4004_9409

LIBC = ctypes.CDLL(None, use_errno=True)
# The platform as a plain string, so a type check on any one platform reads every branch.
PLATFORM: str = sys.platform


def private_bytes(path: Path) -> int:
    """The bytes of `path` that share their blocks with no other file."""
    if PLATFORM == "darwin":
        asked = AttrList(5, 0, ATTR_CMN_RETURNED_ATTRS, 0, 0, 0, ATTR_CMNEXT_PRIVATESIZE)
        buf = ctypes.create_string_buffer(64)
        if LIBC.getattrlist(os.fsencode(path), ctypes.byref(asked), buf, len(buf),
                            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) != 0:
            err = ctypes.get_errno()
            raise engine.Failed(f"getattrlist(ATTR_CMNEXT_PRIVATESIZE) of {path}: {os.strerror(err)} (errno {err})")
        _, _, _, _, _, fork = struct.unpack_from("=I5I", buf, 0)
        if not fork & ATTR_CMNEXT_PRIVATESIZE:
            raise engine.Failed(f"getattrlist of {path}: the volume does not say its private size (not APFS?)")
        return struct.unpack_from("=q", buf, 24)[0]
    if PLATFORM == "linux":
        import fcntl
        size = path.stat().st_size
        private, start = 0, 0
        with open(path, "rb") as f:
            while True:
                buf = bytearray(FIEMAP_HEAD.size + EXTENTS_PER_CALL * FIEMAP_EXTENT.size)
                FIEMAP_HEAD.pack_into(buf, 0, start, (1 << 64) - 1 - start, FIEMAP_FLAG_SYNC, 0, EXTENTS_PER_CALL, 0)
                try:
                    fcntl.ioctl(f.fileno(), FS_IOC_FIEMAP, buf)
                except OSError as e:
                    raise engine.Failed(f"FIEMAP of {path}: {e}") from None
                mapped = FIEMAP_HEAD.unpack_from(buf, 0)[3]
                last = False
                for n in range(mapped):
                    logical, _, length, _, _, flags, _, _, _ = FIEMAP_EXTENT.unpack_from(buf, FIEMAP_HEAD.size + n * FIEMAP_EXTENT.size)
                    end = min(logical + length, size)
                    if not flags & FIEMAP_EXTENT_SHARED and end > logical:
                        private += end - logical
                    start = logical + length
                    last = last or bool(flags & FIEMAP_EXTENT_LAST)
                if last or mapped == 0:
                    return private
    raise engine.Failed(f"no measure of shared blocks on {PLATFORM}")


def measure(root: Path) -> Tree:
    files = size = private = 0
    for top, _, names in os.walk(root):
        for name in names:
            path = Path(top) / name
            if path.is_symlink() or not path.is_file():
                continue
            files += 1
            size += path.stat().st_size
            private += private_bytes(path)
    return Tree(files, size, private)


def sizes(root: Path) -> dict[str, int]:
    """Each regular file under `root` by its path there, with its size."""
    found: dict[str, int] = {}
    for top, _, names in os.walk(root):
        for name in names:
            path = Path(top) / name
            if not path.is_symlink() and path.is_file():
                found[str(path.relative_to(root))] = path.stat().st_size
    return found


async def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], timeout_s: float) -> engine.Ran:
    """`argv` to its end within `timeout_s`; a command that outlives it is a `Failed` naming it."""
    loop = asyncio.get_running_loop()
    began = loop.time()
    try:
        proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
                                                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    except OSError as e:
        raise engine.Failed(f"{step}: `{' '.join(argv)}` in {cwd} did not start: {e}") from None
    try:
        async with asyncio.timeout(timeout_s):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        out, err = await proc.communicate()
        raise engine.Failed(f"{step}: `{' '.join(argv)}` in {cwd} took more than {timeout_s:.0f} s:\n"
                            f"{engine.text(out + err)[-3000:]}") from None
    return engine.Ran(tuple(argv), loop.time() - began, proc.returncode or 0, engine.text(out), engine.text(err))


def placed_of(label: str, ran: engine.Ran) -> dict[str, int]:
    """The `placed` counts of the build's one `blaze-work` line."""
    lines = engine.worked_lines(ran.stdout)
    if len(lines) != 1:
        raise engine.Failed(f"{label}: {len(lines)} `blaze-work` lines, not one:\n{ran.said()}")
    found = lines[0].get("placed")
    if not isinstance(found, dict):
        raise engine.Failed(f"{label}: its `blaze-work` line has no `placed` (API schema 27): {lines[0]}")
    placed = cast(dict[str, object], found)
    counts: dict[str, int] = {}
    for name in ("cloned", "written", "copied"):
        value = placed.get(name)
        if not isinstance(value, int) or value < 0:
            raise engine.Failed(f"{label}: `placed.{name}` is {value!r}, not a count")
        counts[name] = value
    return counts


def build_dirs(home: Path) -> set[Path]:
    root = home / "build"
    return {it for it in root.iterdir() if it.is_dir()} if root.is_dir() else set()


async def check(eng: engine.Engine, rep: report.Report, args: Input, work: Path) -> None:
    state = fixture.default_state() if args.fixture_seed == "base" else fixture.random_state(args.fixture_seed)
    source, home = work / "source", work / "home"
    for path in (source, home, work / "a", work / "b"):
        shutil.rmtree(path, ignore_errors=True)
    source.mkdir(parents=True)
    fixture.write(source, state)
    want = fixture.environment(state)
    extra = {name: value for name in ("ORACLE_SCRIPT_VALUE", "ORACLE_SOURCE_ENV") if (value := want[name]) is not None}
    env = eng.env(home, None, None, extra)
    if want["PATH_EXTRA"]:
        front = work / "path-extra"
        front.mkdir(exist_ok=True)
        env["PATH"] = f"{front}{os.pathsep}{env['PATH']}"
    checkouts = {name: work / name / "ws" for name in ("a", "b")}
    for ws in checkouts.values():
        shutil.copytree(source, ws, symlinks=True)

    async def build(name: str) -> tuple[engine.Ran, Path]:
        before = build_dirs(home)
        step = f"checkout {name}: blaze build"
        ran = await run(step, [str(eng.blaze), "build", "--message-format=json"], checkouts[name], env, args.timeout_s)
        if ran.returncode != 0:
            raise engine.Failed(f"{step} exited {ran.returncode}:\n{ran.said()}")
        made = build_dirs(home) - before
        if len(made) != 1:
            raise engine.Failed(f"{step}: {len(made)} new build directories under {home / 'build'}, not one: "
                                f"{sorted(map(str, made))}")
        return ran, made.pop()

    store = engine.store_of(home)
    try:
        ran_a, dir_a = await build("a")
        store_a = sizes(store)
        ran_b, dir_b = await build("b")
    finally:
        await run("stopping the daemon", [str(eng.blaze), "stop"], checkouts["a"], env, 300)
    store_b = sizes(store)
    async with asyncio.TaskGroup() as group:
        tree_a = group.create_task(asyncio.to_thread(measure, dir_a))
        tree_b = group.create_task(asyncio.to_thread(measure, dir_b))
    a, b = tree_a.result(), tree_b.result()
    placed = sum(size for name, size in store_b.items() if name.startswith("placed/") and name not in store_a)
    grown = sum(max(0, size - store_a.get(name, 0)) for name, size in store_b.items() if not name.startswith("placed/"))
    added = b.private + placed + grown
    limit = max(int(args.most_added_share * b.size), args.most_added_bytes)
    counts_a, counts_b = placed_of("checkout a", ran_a), placed_of("checkout b", ran_b)
    numbers: dict[str, report.Number] = dict(
        build_dir_a_bytes=a.size, build_dir_a_private=a.private, build_dir_b_files=b.files, build_dir_b_bytes=b.size,
        build_dir_b_private=b.private, store_placed_added=placed, store_other_grown=grown, added=added, limit=limit,
        **{f"a_{k}": v for k, v in counts_a.items()}, **{f"b_{k}": v for k, v in counts_b.items()})
    said = (f"the second checkout's build directory holds {b.size} bytes in {b.files} files; it added {added} "
            f"({b.private} private, {placed} in new placed files, {grown} of other store growth), "
            f"limit {limit}; placed by the builds: a {counts_a}, b {counts_b}")
    rep.add(CHECK, "pass" if added <= limit else "fail", said, **numbers)


def engine_of(spec: engine.EngineSpec) -> engine.Engine:
    try:
        return engine.Engine.of(spec)
    except engine.Failed as e:
        raise SystemExit(f"{SCRIPT}: {e}") from None


def main(args: Input) -> None:
    if PLATFORM not in ("darwin", "linux"):
        fail(SCRIPT, f"shared blocks are measured on macOS and Linux, not {PLATFORM}")
    eng = engine_of(args.engine)
    if eng.kind != "v2":
        fail(SCRIPT, "engine.kind: v2; v1 has no build directory under its home and no store it places from")
    rep = report.Report(SCRIPT, args.engine, args.out)
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-clone-out-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    rep.guarded(CHECK, lambda: asyncio.run(check(eng, rep, args, work)))
    rep.extra.update(work=str(work))
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def clone(source: Path, target: Path) -> None:
    if PLATFORM == "darwin":
        if LIBC.clonefile(os.fsencode(source), os.fsencode(target), 0) != 0:
            err = ctypes.get_errno()
            raise SystemExit(f"{SCRIPT} selftest: clonefile {source} to {target}: {os.strerror(err)}")
        return
    import fcntl
    with open(source, "rb") as src, open(target, "wb") as dst:
        try:
            fcntl.ioctl(dst.fileno(), FICLONE, src.fileno())
        except OSError as e:
            raise SystemExit(f"{SCRIPT} selftest: FICLONE {source} to {target}: {e} (no reflink on this volume?)") from None


def selftest(work: str = "") -> None:
    size = 1 << 20
    with tempfile.TemporaryDirectory(dir=work or None) as tmp:
        a, b = Path(tmp) / "a", Path(tmp) / "b"
        a.write_bytes(os.urandom(size))
        with open(a, "rb+") as f:
            os.fsync(f.fileno())
        expect(private_bytes(a) == size, f"a fresh file is private: {private_bytes(a)} of {size}")
        clone(a, b)
        expect(private_bytes(a) == 0 and private_bytes(b) == 0,
               f"a clone shares every block: {private_bytes(a)}, {private_bytes(b)}")
        with open(b, "rb+") as f:
            f.write(os.urandom(8192))
            os.fsync(f.fileno())
        written = private_bytes(b)
        expect(0 < written < size // 4, f"a write into a clone makes only its blocks private: {written}")
        tree = measure(Path(tmp))
        expect(tree.files == 2 and tree.size == 2 * size, f"the tree is both files: {tree}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3) or (len(sys.argv) == 3 and sys.argv[1] != "selftest"):
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest [work dir]\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest(sys.argv[2] if len(sys.argv) == 3 else "")
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
