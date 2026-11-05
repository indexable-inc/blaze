#!/usr/bin/env python3
"""machoimage.py: the Mach-O executable the daemon links, and every splice of it, is one dyld
loads: its load commands fit the room the image keeps for them, it is built for the macOS it
was asked for, with chained fixups as ld-prime gives that macOS, and each build links it once
(macOS on AArch64; issues #13, #30 and #173 of v1, which linked with Apple's linker).

usage: machoimage.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

blaze links with its own linker (crates/blaze-link/src/macho.rs), not Apple's: the room after
the load commands is the image's own (`HEADERS`), not what `-headerpad` says, and a link
argument in RUSTFLAGS reaches no linker. What v1's `-headerpad` and `-no_compact_unwind`
programs guarded is checked here on the file itself, read with the standard library:

- the header and its load commands end before the first section's bytes, in the first link
  and after every splice, also after an edit that links a library the image did not load
  before, which adds a load command (`LC_LOAD_DYLIB`) a splice has to fit;
- `LC_BUILD_VERSION` says macOS and the `MACOSX_DEPLOYMENT_TARGET` the build had (14.0, as
  nix's shell builds), and the image is fixed up by chains (`LC_DYLD_CHAINED_FIXUPS`, no
  `LC_DYLD_INFO_ONLY`), which ld-prime gives macOS 12 and later (#30: a splice declined them);
- `__unwind_info` is there, in `__TEXT`;
- each build links the program once (#173: a program was linked a second time with more
  room), as its `blaze-work` line says (`--message-format=json`, storelib.links); every edit
  after the first is spliced, and the one that adds a library is spliced or laid out in full
  saying why; a daemon started anew links it once too;
- the program prints what the source says.

Python 3, stdlib only.
"""
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import Link, linked  # noqa: E402

os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
PROGRAM = "mimg"
DEPLOYMENT = "14.0"
FILES = {
    "Cargo.toml": f'[package]\nname = "{PROGRAM}"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/main.rs": """fn answer() -> u32 {
    1
}

fn main() {
    println!("{}", answer());
}
""",
}
EDITS = 4
# An edit that calls into a library the image did not load: libz, which every macOS has.
LIBRARY_DECL = """#[link(name = "z")]
unsafe extern "C" {
    fn zlibVersion() -> *const std::ffi::c_char;
}

"""
LIBRARY_USE = '    println!("z {}", !unsafe { zlibVersion() }.is_null());\n'

MH_MAGIC_64 = 0xFEEDFACF
LC_SEGMENT_64 = 0x19
LC_LOAD_DYLIB = 0xC
LC_DYLD_INFO_ONLY = 0x80000022
LC_DYLD_CHAINED_FIXUPS = 0x80000034
LC_BUILD_VERSION = 0x32
PLATFORM_MACOS = 1


@dataclass(frozen=True)
class Image:
    """What a Mach-O executable's header says, as far as this harness reads it."""

    commands: dict[int, int]
    """How many load commands of each kind."""
    headers_end: int
    """Where the header and the load commands end in the file."""
    first_section: int
    """The file offset of the first section that has bytes in the file."""
    sections: frozenset[tuple[str, str]]
    """(segment, section) of every section."""
    platform: int | None
    minimum: int | None
    """`LC_BUILD_VERSION`'s minimum OS, as `xxxx.yy.zz` nibbles (14.0 is 0x000e0000)."""
    dylibs: frozenset[str]
    """The install names of the libraries it loads (`LC_LOAD_DYLIB`)."""


def cstr(raw: bytes) -> str:
    return raw.split(b"\0", 1)[0].decode("ascii", "replace")


def read_image(path: str) -> Image:
    """The header of the thin 64-bit Mach-O executable at `path`."""
    with open(path, "rb") as file:
        data = file.read()
    magic, _, _, _, ncmds, sizeofcmds, _, _ = struct.unpack_from("<IiiIIIII", data, 0)
    if magic != MH_MAGIC_64:
        raise SystemExit(f"{path}: not a thin 64-bit Mach-O file (magic {magic:#x})")
    commands: dict[int, int] = {}
    sections: set[tuple[str, str]] = set()
    offsets: list[int] = []
    dylibs: set[str] = set()
    platform = minimum = None
    at = 32
    for _ in range(ncmds):
        cmd, size = struct.unpack_from("<II", data, at)
        commands[cmd] = commands.get(cmd, 0) + 1
        if cmd == LC_SEGMENT_64:
            nsects = struct.unpack_from("<I", data, at + 64)[0]
            for n in range(nsects):
                section = at + 72 + 80 * n
                name, segment = cstr(data[section:section + 16]), cstr(data[section + 16:section + 32])
                size_of, offset = struct.unpack_from("<QI", data, section + 40)
                sections.add((segment, name))
                # Zero-fill sections (`__bss`, thread-local zeroes) have no bytes in the file.
                if offset and size_of:
                    offsets.append(offset)
        elif cmd == LC_LOAD_DYLIB:
            name_at = struct.unpack_from("<I", data, at + 8)[0]
            dylibs.add(cstr(data[at + name_at:at + size]))
        elif cmd == LC_BUILD_VERSION:
            platform, minimum = struct.unpack_from("<II", data, at + 8)
        at += size
    return Image(commands, 32 + sizeofcmds, min(offsets, default=0), frozenset(sections), platform, minimum,
                 frozenset(dylibs))


def version(text: str) -> int:
    """`14.0` as `LC_BUILD_VERSION` writes it."""
    parts = [int(it) for it in text.split(".")] + [0, 0]
    return parts[0] << 16 | parts[1] << 8 | parts[2]


def image_wrong(path: str, library: str | None) -> str | None:
    """What is wrong with the image at `path` for dyld, or None; `library` is the start of the
    file name of a library it has to load."""
    image = read_image(path)
    if library and not any(os.path.basename(it).startswith(library) for it in image.dylibs):
        return f"DOES NOT LOAD {library}: {sorted(image.dylibs)}"
    if not image.first_section or image.headers_end > image.first_section:
        return f"LOAD COMMANDS ({image.headers_end} bytes) PAST THE FIRST SECTION AT {image.first_section}"
    if image.platform != PLATFORM_MACOS or image.minimum != version(DEPLOYMENT):
        return f"BUILT FOR PLATFORM {image.platform} {image.minimum and hex(image.minimum)}, NOT macOS {DEPLOYMENT}"
    if not image.commands.get(LC_DYLD_CHAINED_FIXUPS) or image.commands.get(LC_DYLD_INFO_ONLY):
        return "NOT FIXED UP BY CHAINS"
    if ("__TEXT", "__unwind_info") not in image.sections:
        return "NO __unwind_info"
    return None


def verdict_of(made: list[Link], program: str, printed: str, want: str, hows: tuple[str, ...],
               library: str | None) -> str:
    """What is wrong with a build that linked `made` and whose program printed `printed`, or
    "ok": `hows` are the ways it may have been linked (none: any), `library` one the image has
    to load."""
    if not made:
        return "NO LINK"
    if len(made) > 1:
        return f"LINKED {len(made)} TIMES: {', '.join(str(it) for it in made)}"
    if printed != want:
        return f"PRINTED {printed!r}, NOT {want!r}"
    if hows and made[0].how not in hows:
        return f"NOT SPLICED: {made[0]}"
    return image_wrong(program, library) or "ok"


def main() -> None:
    if sys.platform != "darwin":
        print("machoimage.py: Mach-O is macOS's; nothing to do")
        return
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    root = tempfile.mkdtemp(prefix="blaze-machoimage-", dir="/tmp")
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               MACOSX_DEPLOYMENT_TARGET=DEPLOYMENT)
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", PROGRAM), os.path.join(ws, "src", "main.rs")
    rows: list[tuple[str, str, str]] = []

    library: str | None = None

    def step(what: str, want: str, hows: tuple[str, ...]) -> None:
        made = linked(ws, env, what, PROGRAM)
        printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
        verdict = verdict_of(made, program, printed, want, hows, library)
        rows.append((what, verdict, ", ".join(str(it) for it in made)))
        print(f"{what:30} {verdict:12} {rows[-1][2][:110]}", flush=True)

    def edit(old: str, new: str) -> None:
        text = open(source).read()
        if old not in text:
            raise SystemExit(f"{source} has no {old!r}")
        open(source, "w").write(text.replace(old, new, 1))

    try:
        step("the first link", "1", ("first",))
        for n in range(1, EDITS + 1):
            edit(f"    {n}\n}}", f"    {n + 1}\n}}")
            # The first edit may write the whole object again.
            step(f"edit {n}", str(n + 1), ("spliced",) if n > 1 else ("spliced", "full"))
        edit("fn main() {\n", "fn main() {\n" + LIBRARY_USE)
        edit("fn answer()", LIBRARY_DECL + "fn answer()")
        library = "libz"
        step("a library it did not load", f"z true\n{EDITS + 1}", ("spliced", "full"))
        # A daemon started anew links once too, from what the last one kept.
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        edit(f"    {EDITS + 1}\n}}", f"    {EDITS + 2}\n}}")
        step("a new daemon", f"z true\n{EDITS + 2}", ())
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    wrong = sum(verdict != "ok" for _, verdict, _ in rows)
    print(f"{len(rows) - wrong} of {len(rows)} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
