"""debug_side.py: a dev build's executable carries no debug info, its side file is found unaided,
and a panic's backtrace names the same functions and file:line as before
(docs/v2/plan/design/debug-side-file.md).

    uv run --frozen python harness/v2/debug_side.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/debug-side.json"}'
    uv run --frozen python harness/v2/debug_side.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine   the blaze measured (engine.EngineSpec)
    out      the result's path (report.py's JSON)
    work     where the program and its daemon's home go; a new directory under /tmp when empty
    keep     keep the work directory (false)

It writes a hello world whose `main` calls `deep`, which panics at a known line, and builds it
twice with a daemon and an empty home each: once as it is (debug info beside the executable, the
default), once with `split-debuginfo = "off"` in `[profile.dev]` (debug info inside). Outcomes:

    sizes        the executable's and the side file's bytes, both ways; fail when the default's
                 executable has a `.debug_*` (ELF) or `__DWARF` (Mach-O) section, or no side file
                 is beside it at each path the executable is at
    build id     the side file carries the executable's build id (ELF: the note; Mach-O: LC_UUID)
                 and, on ELF, has the CRC-32 the executable's `.gnu_debuglink` names
    backtrace    the program run with RUST_BACKTRACE=1 exits 101 and its backtrace has `deep` and
                 `main` frames at `src/main.rs:LINE`, the same frames (function and file:line) in
                 both builds
    gdb, lldb    `info line deep` / `image lookup -n deep -v` resolves `src/main.rs`; "not checked:
                 no gdb" (unmeasured) where the debugger is not installed: never a silent skip

`selftest` checks the backtrace reader, the section readers and the CRC without blaze.
"""

import re
import shutil
import struct
import sys
import zlib
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report

SCRIPT = "debug_side.py"
PANIC_LINE = 6
MAIN = """fn main() {
    deep(std::env::args().count());
}

#[inline(never)]
fn deep(n: usize) { panic!("debug side {n}"); }
"""
MANIFEST = """[package]
name = "debugside"
version = "0.1.0"
edition = "2021"

[workspace]
"""
INSIDE = """
[profile.dev]
split-debuginfo = "off"
"""
FRAME = re.compile(r"^\s*\d+:\s+(?:0x[0-9a-f]+ - )?(?P<function>\S.*?)\s*$")
AT = re.compile(r"^\s+at (?P<file>\S+?):(?P<line>\d+)(?::\d+)?\s*$")


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    keep: bool = False


@dataclass(frozen=True)
class Built:
    """One build: the executable at each path it is at, and what running it printed."""

    executables: tuple[Path, ...]
    stderr: str
    status: int


def frames(stderr: str) -> list[tuple[str, str, int]]:
    """A `RUST_BACKTRACE=1` backtrace's frames that have a place: (function, file, line), the
    function without its hash suffix."""
    out: list[tuple[str, str, int]] = []
    function: str | None = None
    for line in stderr.splitlines():
        if (m := FRAME.match(line)) is not None:
            function = re.sub(r"::h[0-9a-f]{16}$", "", m["function"])
        elif (m := AT.match(line)) is not None and function is not None:
            out.append((function, m["file"], int(m["line"])))
            function = None
    return out


def own_frames(stderr: str) -> list[tuple[str, str, int]]:
    """The frames in the program's own source."""
    return [it for it in frames(stderr) if it[1].endswith("src/main.rs")]


def elf_sections(data: bytes) -> dict[str, tuple[int, int]]:
    """An ELF64 little-endian file's sections by name: (file offset, size)."""
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("not a 64-bit little-endian ELF file")
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
    headers = [struct.unpack_from("<IIQQQQ", data, shoff + n * shentsize) for n in range(shnum)]
    names_at, names_len = headers[shstrndx][4], headers[shstrndx][5]
    names = data[names_at:names_at + names_len]
    out: dict[str, tuple[int, int]] = {}
    for name, kind, _flags, _addr, offset, size in headers:
        if kind != 0:
            out[names[name:names.index(b"\0", name)].decode()] = (offset, size)
    return out


def macho_commands(data: bytes) -> list[tuple[int, bytes]]:
    """A 64-bit Mach-O file's load commands: (command, its bytes)."""
    magic, _cpu, _sub, _kind, count, _size = struct.unpack_from("<IIIIII", data, 0)
    if magic != 0xFEEDFACF:
        raise ValueError("not a 64-bit Mach-O file")
    out: list[tuple[int, bytes]] = []
    at = 32
    for _ in range(count):
        command, size = struct.unpack_from("<II", data, at)
        out.append((command, data[at:at + size]))
        at += size
    return out


def macho_uuid(data: bytes) -> bytes | None:
    return next((body[8:24] for command, body in macho_commands(data) if command == 0x1B), None)


def macho_segments(data: bytes) -> list[str]:
    return [body[8:24].rstrip(b"\0").decode() for command, body in macho_commands(data) if command == 0x19]


def carried(data: bytes) -> list[str]:
    """The sections or segments of an executable that hold debug info."""
    if data[:4] == b"\x7fELF":
        return sorted(name for name in elf_sections(data) if name.startswith(".debug_"))
    return [name for name in macho_segments(data) if name == "__DWARF"]


def side_of(exe: Path, data: bytes) -> Path:
    """Where the executable's side file is looked for: beside it, under the name its
    `.gnu_debuglink` holds (ELF), or in its `.dSYM` bundle (Mach-O)."""
    if data[:4] == b"\x7fELF":
        link = elf_sections(data).get(".gnu_debuglink")
        if link is None:
            raise ValueError("the executable has no .gnu_debuglink")
        body = data[link[0]:link[0] + link[1]]
        return exe.parent / body[:body.index(b"\0")].decode()
    return exe.parent / f"{exe.name}.dSYM" / "Contents" / "Resources" / "DWARF" / exe.name


def same_build(exe: bytes, side: bytes) -> str:
    """Why `side` is not `exe`'s side file; empty when it is."""
    if exe[:4] == b"\x7fELF":
        of_exe, of_side = elf_sections(exe), elf_sections(side)
        if ".note.gnu.build-id" not in of_exe or ".note.gnu.build-id" not in of_side:
            return "a build id note is missing"

        def note(data: bytes, sections: dict[str, tuple[int, int]]) -> bytes:
            at, size = sections[".note.gnu.build-id"]
            return data[at:at + size]

        if note(exe, of_exe) != note(side, of_side):
            return f"the side file's build id {note(side, of_side).hex()} is not the executable's {note(exe, of_exe).hex()}"
        link = of_exe[".gnu_debuglink"]
        crc, = struct.unpack_from("<I", exe, link[0] + link[1] - 4)
        if zlib.crc32(side) != crc:
            return f"the side file's CRC-32 {zlib.crc32(side):#x} is not the {crc:#x} the executable's .gnu_debuglink names"
        return ""
    if macho_uuid(exe) is None or macho_uuid(exe) != macho_uuid(side):
        return f"the side file's LC_UUID {macho_uuid(side)!r} is not the executable's {macho_uuid(exe)!r}"
    return ""


def build(eng: engine.Engine, top: Path, label: str, manifest: str) -> Built:
    """`blaze build` of the program with `manifest`, with an empty home and a daemon of its own,
    then the program run with a backtrace."""
    tree, home = top / label / "src", top / label / "home"
    shutil.rmtree(top / label, ignore_errors=True)
    (tree / "src").mkdir(parents=True)
    (tree / "Cargo.toml").write_text(manifest)
    (tree / "src" / "main.rs").write_text(MAIN)
    env = eng.env(home, None, {})
    try:
        ran = eng.build(f"{label}: build", tree, env)
        if ran.returncode != 0:
            raise engine.Failed(f"{label}: `blaze build` exited {ran.returncode}:\n{ran.said()}")
        if (who := eng.not_blazes(ran)) is not None:
            raise engine.Failed(f"{label}: built by {who}, not blaze")
        target = eng.build_dir(tree, env)
    finally:
        eng.stop(tree, env)
    found = tuple(sorted(path for path in target.rglob("debugside")
                         if path.is_file() and ".dSYM" not in path.parts and "debug" in path.parts))
    if not found:
        raise engine.Failed(f"{label}: no executable `debugside` under {target}")
    top_level = next((path for path in found if path.parent.name == "debug"), found[0])
    program = engine.run(f"{label}: run", [str(top_level)], tree, {**env, "RUST_BACKTRACE": "1"})
    return Built(found, program.stderr, program.returncode)


def debugger(rep: report.Report, name: str, argv: list[str], exe: Path, cwd: Path) -> None:
    """`argv` on the executable resolves `deep` to `src/main.rs`; unmeasured, saying so, where the
    debugger is not installed."""
    if shutil.which(argv[0]) is None:
        rep.add(name, "unmeasured", f"not checked: no {argv[0]}")
        return
    ran = engine.run(name, [*argv, str(exe)], cwd, engine.scrubbed(), timeout=120)
    said = ran.stdout + ran.stderr
    if "src/main.rs" in said and str(PANIC_LINE) in said:
        rep.add(name, "pass", f"`deep` resolves to src/main.rs:{PANIC_LINE}")
    else:
        rep.add(name, "fail", f"`{' '.join(argv)}` did not resolve `deep` to src/main.rs:{PANIC_LINE}:\n{said[-2000:]}")


def main(args: Input) -> None:
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    work = engine.work_dir(args.work, "bz-debug-side-")
    work.mkdir(parents=True, exist_ok=True)

    def measure() -> None:
        beside = build(eng, work, "beside", MANIFEST)
        inside = build(eng, work, "inside", MANIFEST + INSIDE)
        numbers: dict[str, report.Number] = {}
        problems: list[str] = []
        ids: list[str] = []
        for exe in beside.executables:
            data = exe.read_bytes()
            numbers[f"executable {exe.parent.name}"] = len(data)
            if carried(data):
                problems.append(f"{exe} carries {', '.join(carried(data))}")
            side = side_of(exe, data)
            if not side.is_file():
                problems.append(f"{exe} has no side file at {side}")
                continue
            numbers[f"side file {exe.parent.name}"] = side.stat().st_size
            if why := same_build(data, side.read_bytes()):
                ids.append(f"{side}: {why}")
        for exe in inside.executables:
            data = exe.read_bytes()
            numbers[f"executable {exe.parent.name}, debug info inside"] = len(data)
            if not carried(data):
                problems.append(f"{exe}, built with split-debuginfo = \"off\", carries no debug info")
        numbers["executables"] = len(beside.executables)
        rep.add("sizes", "fail" if problems else "pass", "\n".join(problems), **numbers)
        rep.add("build id", "fail" if ids or problems else "pass", "\n".join(ids or problems))
        got, want = own_frames(beside.stderr), own_frames(inside.stderr)
        names = {function.rsplit("::", 1)[-1] for function, _, _ in got}
        placed = any(line == PANIC_LINE and function.endswith("deep") for function, _, line in got)
        if beside.status != 101 or inside.status != 101:
            rep.add("backtrace", "fail", f"the program exited {beside.status} and {inside.status}, not 101:\n{beside.stderr[-2000:]}")
        elif got != want:
            rep.add("backtrace", "fail", f"beside: {got}\ninside: {want}")
        elif not {"deep", "main"} <= names or not placed:
            rep.add("backtrace", "fail", f"no `deep` at src/main.rs:{PANIC_LINE} and `main` among {got}:\n{beside.stderr[-2000:]}")
        else:
            rep.add("backtrace", "pass", f"{len(got)} frames of src/main.rs, the same both ways", frames=len(got))
        exe = beside.executables[-1]
        debugger(rep, "gdb", ["gdb", "-batch", "-ex", "info line deep"], exe, work)
        debugger(rep, "lldb", ["lldb", "--batch", "-o", "image lookup -n deep -v"], exe, work)

    rep.guarded("debug side file", measure)
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    rep.extra.update(work=str(work))
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def test_elf(sections: list[tuple[str, bytes]]) -> bytes:
    """An ELF64 file of `sections`, as far as `elf_sections` reads one."""
    names = b"\0" + b"".join(name.encode() + b"\0" for name, _ in sections) + b".shstrtab\0"
    body, placed = bytearray(64), []
    for name, data in [*sections, (".shstrtab", names)]:
        placed.append((names.index(name.encode() + b"\0"), len(body), len(data)))
        body += data
    shoff = len(body)
    body += bytes(64)
    for name, offset, size in placed:
        body += struct.pack("<IIQQQQIIQQ", name, 1, 0, 0, offset, size, 0, 0, 1, 0)
    body[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", body, 0x28, shoff)
    struct.pack_into("<HHH", body, 0x3A, 64, len(placed) + 1, len(placed))
    return bytes(body)


def selftest() -> None:
    trace = """thread 'main' panicked at src/main.rs:6:21:
debug side 1
stack backtrace:
   0: rust_begin_unwind
             at /rustc/abc/library/std/src/panicking.rs:695:5
   1: core::panicking::panic_fmt
             at /rustc/abc/library/core/src/panicking.rs:75:14
   2: debugside::deep::h0123456789abcdef
             at ./src/main.rs:6:21
   3:     0x55d0c0de - debugside::main
             at ./src/main.rs:2:5
   4: core::ops::function::FnOnce::call_once
note: Some details are omitted
"""
    expect(own_frames(trace) == [("debugside::deep", "./src/main.rs", 6), ("debugside::main", "./src/main.rs", 2)],
           f"the program's frames are read with their places: {own_frames(trace)}")
    expect(len(frames(trace)) == 4, "a frame with no place is left out")
    expect(MAIN.splitlines()[PANIC_LINE - 1].startswith("fn deep"), "PANIC_LINE is the panic's line")
    note = struct.pack("<III", 4, 16, 3) + b"GNU\0" + bytes(range(16))
    side = test_elf([(".note.gnu.build-id", note), (".debug_line", b"rows")])
    link = b"debugside.debug\0" + struct.pack("<I", zlib.crc32(side))
    exe = test_elf([(".note.gnu.build-id", note), (".gnu_debuglink", link), (".text", b"\xc3")])
    expect(carried(exe) == [] and carried(side) == [".debug_line"], "debug sections are found by name")
    expect(side_of(Path("/t/debug/debugside"), exe) == Path("/t/debug/debugside.debug"), "the side file is the one named")
    expect(same_build(exe, side) == "", f"a side file of the executable is its own: {same_build(exe, side)}")
    other = test_elf([(".note.gnu.build-id", note[:-1] + b"\xff"), (".debug_line", b"rows")])
    expect("build id" in same_build(exe, other), "a side file of another build id is caught")
    expect("CRC-32" in same_build(exe, test_elf([(".note.gnu.build-id", note), (".debug_line", b"rowz")])),
           "a side file of another CRC is caught")
    uuid = struct.pack("<II", 0x1B, 24) + bytes(range(16))
    segment = struct.pack("<II16s", 0x19, 72, b"__DWARF") + bytes(48)

    def macho(commands: list[bytes]) -> bytes:
        return struct.pack("<IIIIIIII", 0xFEEDFACF, 0, 0, 2, len(commands), sum(map(len, commands)), 0, 0) + b"".join(commands)

    expect(macho_uuid(macho([uuid, segment])) == bytes(range(16)), "LC_UUID is read")
    expect(carried(macho([uuid, segment])) == ["__DWARF"] and carried(macho([uuid])) == [], "__DWARF is found")
    expect(same_build(macho([uuid]), macho([uuid, segment])) == "", "a dSYM of the executable's UUID is its own")
    expect(side_of(Path("/t/debug/app"), macho([uuid])) == Path("/t/debug/app.dSYM/Contents/Resources/DWARF/app"),
           "the dSYM is beside the executable")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
