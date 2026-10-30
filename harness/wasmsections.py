#!/usr/bin/env python3
"""wasmsections.py: the custom sections of a wasm object blaze writes are rustc's.

A `#[link_section]` static is a custom section of the module on wasm, which wasm-bindgen and
the like read. Builds `harness/wasmsections` with rustc and with blaze for each wasm target
and compares the custom sections of the two objects, leaving out the linker's own and those of
tools. A static in `.init_array` is none: it is an init function of the module (`ctor`'s), and
the priorities of the `WASM_INIT_FUNCS` of the two `linking` sections are compared too.

    uv run --frozen python harness/wasmsections.py '{}'
    uv run --frozen python harness/wasmsections.py '{"targets": ["wasm32-wasip1"]}'

One JSON object, inline, a file's path, or `-` for stdin: `targets` names the wasm targets
(empty runs wasm32-unknown-unknown and wasm32-wasip1). Unknown fields are refused.
"""
import os, subprocess, sys, tempfile
from dataclasses import dataclass

from difftest import BLAZE
from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE = os.path.join(HERE, "wasmsections")
# The linker's, and what tools put in: not the program's.
TOOLS = ("linking", "name", "producers")


def leb(data, at):
    value = shift = 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7f) << shift
        shift += 7
        if byte < 0x80:
            return value, at


def custom_sections(path):
    """The custom sections of a wasm module or object, by name, those of one name joined."""
    data = open(path, "rb").read()
    assert data[:4] == b"\0asm", path
    sections, at = {}, 8
    while at < len(data):
        kind = data[at]
        size, at = leb(data, at + 1)
        end = at + size
        if kind == 0:
            length, start = leb(data, at)
            name = data[start:start + length].decode()
            if name not in TOOLS and not name.startswith(("reloc.", ".debug")):
                sections[name] = sections.get(name, b"") + data[start + length:end]
        at = end
    return sections


def init_priorities(path):
    """The priorities of the init functions the `linking` section of a wasm object lists, sorted."""
    data = open(path, "rb").read()
    at = 8
    while at < len(data):
        kind = data[at]
        size, at = leb(data, at + 1)
        end = at + size
        if kind == 0:
            length, start = leb(data, at)
            if data[start:start + length] == b"linking":
                _, at = leb(data, start + length)
                while at < end:
                    sub = data[at]
                    length, at = leb(data, at + 1)
                    if sub == 6:
                        count, pos = leb(data, at)
                        priorities = []
                        for _ in range(count):
                            priority, pos = leb(data, pos)
                            _, pos = leb(data, pos)
                            priorities.append(priority)
                        return sorted(priorities)
                    at += length
                return []
        at = end
    return []


def sysroot_rlibs(target):
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    lib = os.path.join(sysroot, "lib", "rustlib", target, "lib")
    return [os.path.join(lib, n) for n in sorted(os.listdir(lib)) if n.endswith(".rlib")]


def check(target, work):
    theirs, ours = os.path.join(work, f"{target}.rustc.o"), os.path.join(work, f"{target}.blaze.o")
    subprocess.run(["rustc", "--edition", "2024", "--crate-type", "lib", "--crate-name", "wasmsections",
                    "--emit", f"obj={theirs}", "--target", target, os.path.join(FIXTURE, "src/lib.rs")],
                   check=True)
    symbols = [x for rlib in sysroot_rlibs(target) for x in ("--symbols", rlib)]
    run = subprocess.run([BLAZE, FIXTURE, "--crate", "wasmsections", "--out", ours, "--target", target, *symbols],
                         capture_output=True, text=True)
    if run.returncode != 0:
        print(f"{target}: blaze failed\n{run.stderr[-3000:]}")
        return False
    want, got = custom_sections(theirs), custom_sections(ours)
    want_inits, got_inits = init_priorities(theirs), init_priorities(ours)
    if want == got and want_inits == got_inits:
        print(f"{target}: {len(want)} custom sections and init functions {want_inits}, as rustc's")
        return True
    print(f"{target}: DIFFERENT")
    for name in sorted(set(want) | set(got)):
        if want.get(name) != got.get(name):
            print(f"  {name}: rustc {want.get(name)!r}, blaze {got.get(name)!r}")
    if want_inits != got_inits:
        print(f"  init functions' priorities: rustc {want_inits}, blaze {got_inits}")
    return False


@dataclass(frozen=True)
class Input:
    targets: tuple[str, ...] = ()


def main():
    match sys.argv[1:]:
        case [text]:
            options = parse(Input, text, "wasmsections.py")
        case _:
            fail("wasmsections.py", "usage: wasmsections.py JSON (see the top of the file)")
    os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
    targets = options.targets or ("wasm32-unknown-unknown", "wasm32-wasip1")
    with tempfile.TemporaryDirectory(prefix="wasmsections-") as work:
        right = sum(check(target, work) for target in targets)
    print(f"{right} of {len(targets)} right")
    sys.exit(right != len(targets))


if __name__ == "__main__":
    main()
