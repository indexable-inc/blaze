#!/usr/bin/env python3
"""coldimage.py: hello patched onto an empty ELF image by `blaze-engine patch-image`, against
the same objects linked by lld (docs/patching.md, stage 1).

usage: coldimage.py [INPUT]

INPUT is one JSON object (inline, a file's path, or `-` for stdin; none is `{}`):

    {"blaze": "the `blaze` to build hello with; `blaze-engine` beside it patches (default: PATH's)",
     "work": "a directory to work in (default: a new temporary one)",
     "keep": false}

blaze builds hello with a `cc` of this harness first on PATH, which writes down the line of the
program's link (the one with `-pie` and `-nodefaultlibs`), copies its inputs and links as the
real `cc` does, with lld. `cc -###` turns that line into the one `collect2` runs, and that into
a `patch-image` request: each flag of it is one this harness knows the meaning of, and any
other is an error that names it. Both images then run, and their standard output, standard
error and exit status must be the same. Then the symbolic diff, which holds each image to the
inputs rather than to the other's addresses:

- every relocation of each input section the two images both have (found by a symbol of it
  whose name is the only one of its kind) reaches the same thing in both: a symbol and an
  offset into it, an import by its version, or for what has no symbol, the bytes it points at.
  A GOT slot or a PLT stub counts as what it holds, so a relaxed load and a slot are the same;
- the bytes of those sections are the same outside what the relocations fill;
- the same NEEDED, the same versioned imports, PT_TLS of the same size and alignment, the same
  `.init_array` and `.fini_array` in order, an FDE for the same functions over the same length,
  the same load-time relocations by what they reach, PT_INTERP, BIND_NOW, PIE and GNU_STACK.

lld is in the harness only: no product path runs it in place of the patch. Linux on x86-64 and
AArch64; it runs where it is. Stdlib only.
"""
import asyncio, bisect, json, os, shlex, shutil, struct, sys, tempfile, typing
from collections import Counter
from dataclasses import dataclass, fields

HERE = os.path.dirname(os.path.abspath(__file__))
STEP_SECS = 1800
RUN_SECS = 60

HELLO = """\
fn main() {
    println!("hello, world");
    std::process::exit(3);
}
"""

# The `cc` blaze links with, first on PATH: it writes the line down when it is the program's,
# copies the files the line names, and runs the real `cc` (the next one on PATH).
SHIM = """\
#!/usr/bin/env python3
import json, os, shutil, sys
work = {work!r}
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
path = [p for p in os.environ.get("PATH", "").split(os.pathsep) if p and os.path.abspath(p) != here]
real = shutil.which("cc", path=os.pathsep.join(path))
if real is None:
    sys.exit("coldimage.py's cc: no cc on PATH after this one")
if "-pie" in args and "-nodefaultlibs" in args:
    inputs = os.path.join(work, "inputs")
    os.makedirs(inputs, exist_ok=True)
    line = []
    for i, arg in enumerate(args):
        if not arg.startswith("-") and args[i - 1] != "-o" and os.path.isfile(arg):
            copy = os.path.join(inputs, f"{{len(os.listdir(inputs))}}-{{os.path.basename(arg)}}")
            shutil.copyfile(arg, copy)
            arg = copy
        line.append(arg)
    with open(os.path.join(work, "line.json"), "w") as f:
        json.dump({{"cc": real, "args": line}}, f)
os.execv(real, [real, *args])
"""


@dataclass(frozen=True)
class Input:
    blaze: str = ""
    work: str = ""
    keep: bool = False


def typed(text: str) -> Input:
    """`Input` from INPUT; exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        with open(text) as f:
            text = f.read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"coldimage.py: INPUT is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("coldimage.py: INPUT is a JSON object")
    hints = typing.get_type_hints(Input)
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"coldimage.py: INPUT has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    for name, given in value.items():
        if not isinstance(given, hints[name]):
            sys.exit(f"coldimage.py: INPUT field {name} is a {hints[name].__name__}, not {given!r}")
    return Input(**value)


class Failed(Exception):
    pass


async def run(step: str, argv: list[str], cwd: str | None = None, env: dict | None = None,
              secs: int = STEP_SECS, check: bool = True) -> tuple[int, bytes, bytes]:
    """Runs `argv`; with `check`, a failure names the step, the command, its status and the
    end of its standard error."""
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
                                                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise Failed(f"{step}: no answer after {secs} s: {shlex.join(argv)}")
    code = proc.returncode
    assert code is not None
    if check and code != 0:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-40:])
        raise Failed(f"{step}: exit {code}: {shlex.join(argv)}\n{tail}")
    return code, out, err


# --- The request, from collect2's line ----------------------------------------------------

# Flags of collect2's line that say what the image is anyway: a PIE (`-pie`), bound at load
# (`-z now`), with RELRO, no executable stack, `.eh_frame_hdr`, a build ID, a GNU hash table, and
# without what nothing reaches. Each is what `patch-image` writes whatever it is told.
SAID_ANYWAY = {"-pie", "--eh-frame-hdr", "--build-id", "--hash-style=gnu", "--gc-sections", "-EL", "-fuse-ld=lld",
               # Every path of the line is absolute.
               "--sysroot=/",
               # `.L` labels left out of the symbol table, which the diff does not compare.
               "-X"}
Z_SAID_ANYWAY = {"now", "relro", "noexecstack"}
EMULATIONS = {"x86_64": "elf_x86_64", "aarch64": "aarch64linux"}


def request_of(collect2: list[str], machine: str, output: str) -> tuple[dict, list[str]]:
    """The `patch-image` request of collect2's arguments, and the flags that were taken as
    they are for a reason this harness checks itself (the AArch64 erratum fix)."""
    inputs, search, interpreter = [], [], None
    as_needed = whole = False
    checked = []
    args = iter(collect2)
    for arg in args:
        if arg == "-plugin":
            next(args)  # GCC's LTO plugin: no input of the line is GIMPLE (patch-image refuses one that is not ELF).
        elif arg.startswith("-plugin-opt=") or arg in SAID_ANYWAY:
            pass
        elif arg == "-z":
            z = next(args)
            if z not in Z_SAID_ANYWAY:
                raise Failed(f"collect2's line: `-z {z}`, which coldimage.py does not know the meaning of")
        elif arg.startswith("-m"):
            emulation = arg[2:] or next(args)
            if emulation != EMULATIONS[machine]:
                raise Failed(f"collect2's line: `-m {emulation}` for an image of {machine}")
        elif arg == "-dynamic-linker":
            interpreter = next(args)
        elif arg == "-o":
            next(args)
        elif arg == "-L":
            search.append(next(args))
        elif arg.startswith("-L"):
            search.append(arg[2:])
        elif arg == "-l":
            inputs.append({"lib": next(args), "as_needed": as_needed, "whole": whole})
        elif arg.startswith("-l"):
            inputs.append({"lib": arg[2:], "as_needed": as_needed, "whole": whole})
        elif arg in ("--as-needed", "--no-as-needed"):
            as_needed = arg == "--as-needed"
        elif arg in ("--whole-archive", "--no-whole-archive"):
            whole = arg == "--whole-archive"
        elif arg == "--fix-cortex-a53-843419" and machine == "aarch64":
            # lld rewrites the sequences the erratum hits; patch-image does not yet. The diff
            # checks that lld found none in hello, so the two images mean the same.
            checked.append(arg)
        elif arg.startswith("-"):
            raise Failed(f"collect2's line: `{arg}`, which coldimage.py does not know the meaning of")
        else:
            inputs.append({"path": arg, "as_needed": as_needed, "whole": whole})
    if interpreter is None:
        raise Failed("collect2's line has no -dynamic-linker")
    return {"machine": machine, "output": output, "interpreter": interpreter, "search": search,
            "inputs": inputs, "debug_info": "leave_out"}, checked


def collect2_of(said: str) -> list[str]:
    """collect2's arguments (without its name) in what `cc -###` printed."""
    for line in said.splitlines():
        if line.startswith(" "):
            argv = shlex.split(line)
            if os.path.basename(argv[0]) in ("collect2", "ld", "ld.lld"):
                return argv[1:]
    raise Failed(f"`cc -###` printed no line of collect2's:\n{said}")


# --- ELF ----------------------------------------------------------------------------------

SHT_SYMTAB, SHT_RELA, SHT_NOBITS, SHT_DYNSYM, SHT_REL = 2, 4, 8, 11, 9
SHF_ALLOC = 2
PT_LOAD, PT_DYNAMIC, PT_INTERP, PT_TLS, PT_GNU_STACK, PT_GNU_RELRO = 1, 2, 3, 7, 0x6474E551, 0x6474E552
STT_SECTION, STT_FILE, STT_TLS = 3, 4, 6


@dataclass
class Sec:
    index: int
    name: str
    type: int
    flags: int
    addr: int
    offset: int
    size: int
    link: int
    info: int
    entsize: int


@dataclass
class Sym:
    name: str
    value: int
    size: int
    info: int
    shndx: int
    version: str = ""

    @property
    def type(self) -> int:
        return self.info & 0xF

    @property
    def bind(self) -> int:
        return self.info >> 4


class Elf:
    def __init__(self, name: str, data: bytes):
        self.name, self.data = name, data
        if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
            raise Failed(f"{name}: not a little-endian 64-bit ELF file")
        (self.type, self.machine, _, self.entry, phoff, shoff, _, _, phentsize, phnum, shentsize, shnum,
         shstrndx) = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
        self.segments = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize) for i in range(phnum)]
        raw = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)]
        names = raw[shstrndx][4] if shnum else 0
        self.sections = [Sec(i, self.cstr(names + r[0]) if shnum else "", r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[9])
                         for i, r in enumerate(raw)]

    def cstr(self, at: int) -> str:
        return self.data[at:self.data.index(b"\0", at)].decode(errors="replace")

    def section(self, name: str) -> Sec | None:
        return next((s for s in self.sections if s.name == name), None)

    def symbols(self, sh_type: int) -> list[Sym]:
        table = next((s for s in self.sections if s.type == sh_type), None)
        if table is None:
            return []
        strings = self.sections[table.link].offset
        out = []
        for i in range(table.size // 24):
            name, info, _, shndx, value, size = struct.unpack_from("<IBBHQQ", self.data, table.offset + i * 24)
            out.append(Sym(self.cstr(strings + name), value, size, info, shndx))
        return out

    def relocations(self, section: Sec) -> list[tuple[int, int, int, int]]:
        """(offset, type, symbol index, addend) of a SHT_RELA section."""
        if section.type == SHT_REL:
            raise Failed(f"{self.name}: {section.name} is SHT_REL, which neither machine's objects have")
        out = []
        for i in range(section.size // 24):
            offset, info, addend = struct.unpack_from("<QQq", self.data, section.offset + i * 24)
            out.append((offset, info & 0xFFFFFFFF, info >> 32, addend))
        return out


def members(name: str, data: bytes) -> list[tuple[str, bytes]]:
    """The ELF members of an archive, or the file itself."""
    if not data.startswith(b"!<arch>\n"):
        return [(name, data)] if data.startswith(b"\x7fELF") else []
    out, at, longnames = [], 8, b""
    while at + 60 <= len(data):
        header = data[at:at + 60]
        size = int(header[48:58].decode().strip())
        body = data[at + 60:at + 60 + size]
        ident = header[:16].decode().strip()
        if ident == "//":
            longnames = body
        elif ident.startswith("/") and ident[1:].isdigit():
            start = int(ident[1:])
            ident = longnames[start:longnames.index(b"/\n", start)].decode()
        if body.startswith(b"\x7fELF"):
            out.append((f"{name}({ident.rstrip('/')})", body))
        at += 60 + size + (size & 1)
    return out


# --- What an address in an image is ---------------------------------------------------------

RELATIVE = {62: 8, 183: 1027}  # e_machine -> R_*_RELATIVE
BIND = {62: {6, 7}, 183: {1025, 1026}}  # GLOB_DAT, JUMP_SLOT
ABS64 = {62: 1, 183: 257}
TPOFF64 = {62: 18, 183: 1030}


class Image:
    """An image, read for what its addresses are."""

    def __init__(self, label: str, elf: Elf):
        self.label, self.elf = label, elf
        self.syms = [s for s in elf.symbols(SHT_SYMTAB) if s.name and s.type not in (STT_SECTION, STT_FILE) and s.shndx != 0]
        self.count = Counter(s.name for s in self.syms)
        self.by_name = {s.name: s for s in self.syms}
        placed = sorted((s for s in self.syms if s.type != STT_TLS and s.shndx < 0xFF00), key=lambda s: (s.value, -s.size))
        self.starts = [s.value for s in placed]
        self.placed = placed
        self.dynsym = elf.symbols(SHT_DYNSYM)
        self.versions()
        self.dynrel: dict[int, tuple[int, int, int]] = {}
        self.dynrels = []
        for sec in elf.sections:
            if sec.type == SHT_RELA and sec.flags & SHF_ALLOC:
                for offset, r_type, sym, addend in elf.relocations(sec):
                    self.dynrel[offset] = (r_type, sym, addend)
                    self.dynrels.append((offset, r_type, sym, addend))
        self.loaded = [s for s in elf.sections if s.flags & SHF_ALLOC and s.addr]

    def versions(self) -> None:
        """Each dynamic symbol's version, from `.gnu.version` and `.gnu.version_r`."""
        versym, verneed = self.elf.section(".gnu.version"), self.elf.section(".gnu.version_r")
        if versym is None:
            return
        names = {}
        if verneed is not None:
            strings = self.elf.sections[verneed.link].offset
            at = verneed.offset
            while True:
                _, cnt, _, aux, nxt = struct.unpack_from("<HHIII", self.elf.data, at)
                a = at + aux
                for _ in range(cnt):
                    _, _, other, name, anext = struct.unpack_from("<IHHII", self.elf.data, a)
                    names[other] = self.elf.cstr(strings + name)
                    a += anext
                if not nxt:
                    break
                at += nxt
        for i, sym in enumerate(self.dynsym):
            (index,) = struct.unpack_from("<H", self.elf.data, versym.offset + 2 * i)
            sym.version = names.get(index & 0x7FFF, "")

    def section_at(self, addr: int) -> Sec | None:
        return next((s for s in self.loaded if s.addr <= addr < s.addr + max(s.size, 1)), None)

    def read(self, addr: int, size: int) -> bytes:
        sec = self.section_at(addr)
        if sec is None:
            raise Failed(f"{self.label}: {addr:#x} is in no section")
        if sec.type == SHT_NOBITS:
            return bytes(size)
        at = sec.offset + addr - sec.addr
        return self.elf.data[at:at + size]

    def u64(self, addr: int) -> int:
        return struct.unpack("<Q", self.read(addr, 8))[0]

    def import_name(self, index: int) -> str:
        sym = self.dynsym[index]
        return f"{sym.name}@{sym.version}" if sym.version else sym.name

    def slot(self, addr: int, depth: int) -> str:
        """What a slot of 8 bytes holds once loaded."""
        if addr in self.dynrel:
            r_type, sym, addend = self.dynrel[addr]
            m = self.elf.machine
            if r_type == RELATIVE[m]:
                return self.what(addend, depth + 1)
            if r_type in BIND[m] or r_type == ABS64[m]:
                return self.import_name(sym) + (f"+{addend:#x}" if addend else "")
            if r_type == TPOFF64[m]:
                return f"tpoff({self.import_name(sym)}{addend:+#x})" if sym else f"tpoff({addend:#x})"
            return f"dynamic relocation {r_type}"
        return f"{self.u64(addr):#x}"

    def stub(self, addr: int, depth: int) -> str:
        """What a stub that jumps through a slot goes to: what the slot holds."""
        code = self.read(addr, 16)
        if self.elf.machine == 62:
            at = code.find(b"\xff\x25")
            if at < 0:
                raise Failed(f"{self.label}: the stub at {addr:#x} has no `jmp *slot(%rip)`: {code.hex()}")
            (disp,) = struct.unpack_from("<i", code, at + 2)
            return self.slot(addr + at + 6 + disp, depth)
        adrp, ldr = struct.unpack_from("<II", code)
        if adrp & 0x9F000000 != 0x90000000 or ldr & 0xFFC00000 != 0xF9400000:
            raise Failed(f"{self.label}: the stub at {addr:#x} is not `adrp; ldr`: {code.hex()}")
        return self.slot(adrp_page(adrp, addr) + ((ldr >> 10) & 0xFFF) * 8, depth)

    def what(self, addr: int, depth: int = 0) -> str:
        """What is at `addr`, without its address: a symbol and an offset into it, what a slot
        or a stub holds, or for what has no symbol, its section and its first bytes."""
        sec = self.section_at(addr)
        if sec is not None and sec.name in (".got", ".got.plt"):
            return self.slot(addr, depth)
        if sec is not None and sec.name.startswith(".plt"):
            return self.stub(addr, depth)
        i = bisect.bisect_right(self.starts, addr) - 1
        while i >= 0 and self.starts[i] <= addr:
            s = self.placed[i]
            if s.value == addr or s.value <= addr < s.value + s.size:
                name = s.name if self.count[s.name] == 1 else f"{s.name} (one of {self.count[s.name]})"
                return name if s.value == addr else f"{name}+{addr - s.value:#x}"
            if self.starts[i] < addr - (1 << 20):
                break
            i -= 1
        if sec is None:
            return f"{addr:#x}, in no section"
        if addr == sec.addr + sec.size:
            return f"the end of {sec.name}"
        if depth > 1:
            return f"in {sec.name}"
        end = min(addr + 16, sec.addr + sec.size)
        words = []
        for at in range(addr, end, 8):
            if at in self.dynrel:
                words.append(f"[{self.slot(at, depth)}]")
            else:
                words.append(self.read(at, min(8, end - at)).hex())
        return f"in {sec.name}: {' '.join(words)}"


def adrp_page(word: int, at: int) -> int:
    imm = ((word >> 29) & 3) | (((word >> 5) & 0x7FFFF) << 2)
    if imm & (1 << 20):
        imm -= 1 << 21
    return (at & ~0xFFF) + (imm << 12)


def sext(value: int, bits: int) -> int:
    return value - (1 << bits) if value & (1 << (bits - 1)) else value


# --- Relocations of the inputs, in each image ------------------------------------------------

# x86-64
X_PC32, X_PLT32, X_GOTPCREL, X_GOTPCRELX, X_REX_GOTPCRELX, X_64, X_PC64 = 2, 4, 9, 41, 42, 1, 24
X_TLS = {19: "TLSGD", 20: "TLSLD", 21: "DTPOFF32", 22: "GOTTPOFF", 23: "TPOFF32"}
X_NONE = 0
# AArch64
A_ABS64, A_PREL32, A_PREL64 = 257, 261, 260
A_CALL = {282, 283}
A_PAGE, A_GOT_PAGE = 275, 311
A_LO12 = {277: 0, 278: 0, 284: 1, 285: 2, 286: 3, 299: 4}  # ADD, LDST8..LDST128: the scale
A_GOT_LO12 = 312
A_TLS = set(range(512, 574))  # the thread-local relocations, TLSGD to TLSDESC
A_NONE = 0


def reached(img: Image, base: int, rel: tuple[int, int, int, int], target_name: str, pairs: dict) -> str | None:
    """What one relocation reaches in `img`, its section at `base`; None for one whose meaning
    is in the bytes alone (thread-locals), which the byte comparison checks."""
    offset, r_type, _, addend = rel
    p = base + offset
    if img.elf.machine == 62:
        if r_type == X_NONE or r_type in X_TLS:
            return None
        if r_type in (X_PC32, X_PLT32):
            (field,) = struct.unpack("<i", img.read(p, 4))
            return img.what(p + field - addend)
        if r_type in (X_GOTPCREL, X_GOTPCRELX, X_REX_GOTPCRELX):
            # The opcode and ModRM byte before the field.
            op = img.read(p - 2, 2)
            if op[0] == 0x8B or op in (b"\xff\x15", b"\xff\x25"):
                (field,) = struct.unpack("<i", img.read(p, 4))
                return img.slot(p + field - addend, 0)
            if op[0] == 0x8D or op == b"\x67\xe8":
                (field,) = struct.unpack("<i", img.read(p, 4))
                return img.what(p + field - addend)
            if op[0] == 0xE9:
                # `jmp *x@GOTPCREL(%rip)` relaxed to `jmp x; nop`: the field is a byte earlier.
                (field,) = struct.unpack("<i", img.read(p - 1, 4))
                return img.what(p + 3 + field)
            raise Failed(f"{img.label}: a GOTPCREL relocation at {p:#x} after {op.hex()}, which coldimage.py does not read")
        if r_type == X_64:
            return img.slot(p, 0) if p in img.dynrel else img.what(img.u64(p) - addend)
        if r_type == X_PC64:
            (field,) = struct.unpack("<q", img.read(p, 8))
            return img.what(p + field - addend)
        raise Failed(f"{img.label}: x86-64 relocation type {r_type} ({target_name}), which coldimage.py does not read")
    (word,) = struct.unpack("<I", img.read(p, 4)) if r_type not in (A_ABS64, A_PREL64) else (0,)
    if r_type == A_NONE or r_type in A_TLS:
        return None
    if r_type in A_CALL:
        return img.what(p + sext(word & 0x3FFFFFF, 26) * 4 - addend)
    if r_type in (A_PAGE, A_GOT_PAGE):
        pairs[(rel[2], addend, r_type)] = adrp_page(word, p)
        return "page"
    if r_type in A_LO12:
        page = pairs.get((rel[2], addend, A_PAGE))
        if page is None:
            raise Failed(f"{img.label}: a low-12 relocation at {p:#x} ({target_name}) with no page before it")
        return img.what(page + (((word >> 10) & 0xFFF) << A_LO12[r_type]) - addend)
    if r_type == A_GOT_LO12:
        page = pairs.get((rel[2], addend, A_GOT_PAGE))
        if page is None:
            raise Failed(f"{img.label}: a GOT low-12 relocation at {p:#x} ({target_name}) with no page before it")
        if word & 0xFFC00000 == 0xF9400000:
            return img.slot(page + ((word >> 10) & 0xFFF) * 8 - addend, 0)
        if word & 0xFF800000 == 0x91000000:
            return img.what(page + ((word >> 10) & 0xFFF) - addend)
        raise Failed(f"{img.label}: a GOT low-12 relocation at {p:#x} on {word:#010x}, which coldimage.py does not read")
    if r_type == A_ABS64:
        return img.slot(p, 0) if p in img.dynrel else img.what(img.u64(p) - addend)
    if r_type == A_PREL32:
        return img.what(p + sext(word, 32) - addend)
    if r_type == A_PREL64:
        (field,) = struct.unpack("<q", img.read(p, 8))
        return img.what(p + field - addend)
    raise Failed(f"{img.label}: AArch64 relocation type {r_type} ({target_name}), which coldimage.py does not read")


def masked(machine: int, offset: int, r_type: int) -> range:
    """The bytes a relocation fills, with what a linker may rewrite around it (a relaxed load
    through the GOT). A thread-local's bytes are not masked: both images' must be the same,
    rewritten sequences and offsets from the thread pointer too."""
    if machine == 62:
        if r_type in X_TLS:
            return range(0)
        if r_type in (X_GOTPCREL, X_GOTPCRELX, X_REX_GOTPCRELX):
            return range(offset - 2, offset + 4)
        return range(offset, offset + (8 if r_type in (X_64, X_PC64) else 4))
    if r_type in A_TLS:
        return range(0)
    if r_type == A_GOT_LO12:
        return range(offset, offset + 4)
    return range(offset, offset + (8 if r_type in (A_ABS64, A_PREL64) else 4))


def anchored(objects: list[Elf], images: list[Image]) -> tuple[list[tuple[Elf, list[Sym], Sec, Sym]], int]:
    """Each input section with something to compare, with a symbol of it whose name is the
    only one of its kind in the inputs and in each image (so both images have the section),
    and how many sections of the inputs have none."""
    defined = Counter()
    per = []
    for obj in objects:
        syms = obj.symbols(SHT_SYMTAB)
        per.append(syms)
        for s in syms:
            if s.name and s.type not in (STT_SECTION, STT_FILE) and 0 < s.shndx < 0xFF00:
                defined[s.name] += 1
    out, left = [], 0
    for obj, syms in zip(objects, per):
        by_section: dict[int, Sym] = {}
        for s in syms:
            if (s.name and s.type not in (STT_SECTION, STT_FILE, STT_TLS) and 0 < s.shndx < 0xFF00
                    and defined[s.name] == 1 and all(img.count.get(s.name) == 1 for img in images)):
                by_section.setdefault(s.shndx, s)
        for sec in obj.sections:
            if not sec.flags & SHF_ALLOC or sec.name == ".eh_frame" or sec.type == 7:  # SHT_NOTE
                continue
            if (anchor := by_section.get(sec.index)) is None:
                left += 1
                continue
            out.append((obj, syms, sec, anchor))
    return out, left


def compare_sections(objects: list[Elf], ours: Image, lld: Image) -> list[str]:
    """Every relocation of every anchored input section, in both images, and the bytes around."""
    diffs = []
    sections, left = anchored(objects, [ours, lld])
    rela = {id(obj): {rs.info: rs for rs in obj.sections if rs.type == SHT_RELA} for obj in objects}
    checked = 0
    for obj, syms, sec, anchor in sections:
        rs = rela[id(obj)].get(sec.index)
        rels = obj.relocations(rs) if rs else []
        bases = [img.by_name[anchor.name].value - anchor.value for img in (ours, lld)]
        pairs: list[dict] = [{}, {}]
        mask = set()
        after_tls_call = False
        for rel in rels:
            # The call of `__tls_get_addr` after a general- or local-dynamic access, which a
            # linker rewrites with it: its bytes are compared as they are.
            if after_tls_call:
                after_tls_call = False
                continue
            after_tls_call = obj.machine == 62 and rel[1] in (19, 20)
            mask.update(masked(obj.machine, rel[0], rel[1]))
            target = syms[rel[2]] if rel[2] < len(syms) else None
            name = (target.name or f"section {obj.sections[target.shndx].name}") if target else "?"
            got = [reached(img, base, rel, name, pair) for img, base, pair in zip((ours, lld), bases, pairs)]
            checked += 1
            if got[0] != got[1]:
                diffs.append(f"{anchor.name}+{rel[0] - anchor.value:#x} (type {rel[1]}, to {name}{rel[3]:+#x}): "
                             f"ours reaches {got[0]}, lld's {got[1]}")
        if sec.type != SHT_NOBITS:
            a, b = ours.read(bases[0], sec.size), lld.read(bases[1], sec.size)
            wrong = [i for i in range(sec.size) if a[i] != b[i] and i not in mask]
            if wrong:
                diffs.append(f"{anchor.name} ({obj.name} {sec.name}): {len(wrong)} bytes differ, the first at "
                             f"+{wrong[0] - anchor.value:#x}: ours {a[wrong[0]:wrong[0] + 8].hex()}, lld's {b[wrong[0]:wrong[0] + 8].hex()}")
    print(f"  {len(sections)} input sections in both images, {checked} relocations of them compared; "
          f"{left} sections not compared (taken by neither, or no symbol of theirs has a name of its own)", flush=True)
    return diffs


# --- The rest of the image -------------------------------------------------------------------

def dynamic(img: Image) -> dict[int, list[int]]:
    sec = img.elf.section(".dynamic")
    out: dict[int, list[int]] = {}
    if sec is None:
        return out
    for i in range(sec.size // 16):
        tag, val = struct.unpack_from("<qQ", img.elf.data, sec.offset + 16 * i)
        if tag == 0:
            break
        out.setdefault(tag, []).append(val)
    return out


def frames(img: Image) -> set[tuple[str, int]]:
    """Each FDE of `.eh_frame_hdr`'s table: the function it starts at and its length."""
    hdr = img.elf.section(".eh_frame_hdr")
    if hdr is None:
        raise Failed(f"{img.label}: no .eh_frame_hdr")
    d = img.elf.data[hdr.offset:hdr.offset + hdr.size]
    version, ptr_enc, count_enc, table_enc = d[:4]
    if (version, ptr_enc, count_enc, table_enc) != (1, 0x1B, 0x03, 0x3B):
        raise Failed(f"{img.label}: .eh_frame_hdr's encodings {d[:4].hex()}, not the 011b033b both linkers write")
    (count,) = struct.unpack_from("<I", d, 8)
    out = set()
    for i in range(count):
        start, fde = struct.unpack_from("<ii", d, 12 + 8 * i)
        fde_addr = hdr.addr + fde
        # The FDE's pc_begin is pc-relative sdata4 and its range udata4, as the CIEs of both
        # machines' code generators say ('zR' with 0x1b).
        (length,) = struct.unpack("<I", img.read(fde_addr + 12, 4))
        out.add((img.what(hdr.addr + start), length))
    return out


def arrays(img: Image, name: str) -> list[str]:
    sec = img.elf.section(name)
    if sec is None:
        return []
    return [img.slot(sec.addr + i, 0) for i in range(0, sec.size, 8)]


def segment(img: Image, kind: int):
    return next((p for p in img.elf.segments if p[0] == kind), None)


def facts(img: Image) -> dict[str, object]:
    """What of the image is compared outside its sections' bytes."""
    e, m = img.elf, img.elf.machine
    dyn = dynamic(img)
    dynstr = e.sections[e.section(".dynamic").link].offset if e.section(".dynamic") else 0
    tls = segment(img, PT_TLS)
    interp = segment(img, PT_INTERP)
    stack = segment(img, PT_GNU_STACK)
    load_time = Counter()
    bound = set()
    for offset, r_type, sym, addend in img.dynrels:
        kind = ("relative" if r_type == RELATIVE[m] else "bind" if r_type in BIND[m] else
                "abs64" if r_type == ABS64[m] else "tpoff64" if r_type == TPOFF64[m] else f"type {r_type}")
        target = img.what(addend) if kind == "relative" else img.import_name(sym) if sym else f"{addend:#x}"
        if kind == "bind":
            # GLOB_DAT or JUMP_SLOT: under BIND_NOW both are bound at load, whichever table.
            bound.add(target)
            continue
        sec = img.section_at(offset)
        # A slot of the GOT is one whichever table it is in; anywhere else it is named, or
        # in a piece with no symbol.
        place = ("a slot" if sec is not None and sec.name in (".got", ".got.plt") else
                 "unnamed" if img.what(offset, 2).startswith("in ") else img.what(offset, 2))
        load_time[(kind, place, target)] += 1
    return {
        "e_type": e.type,
        "entry": img.what(e.entry),
        "PT_INTERP": e.data[interp[1]:interp[1] + interp[5]].rstrip(b"\0").decode() if interp else None,
        "PT_TLS (filesz, memsz, align)": (tls[5], tls[6], tls[7]) if tls else None,
        "PT_GNU_STACK flags": stack[1] if stack else None,
        "PT_GNU_RELRO": segment(img, PT_GNU_RELRO) is not None,
        "NEEDED": [e.cstr(dynstr + v) for v in dyn.get(1, [])],
        "DT_FLAGS BIND_NOW": bool(dyn.get(30, [0])[0] & 8),
        "DT_FLAGS_1 NOW|PIE": dyn.get(0x6FFFFFFB, [0])[0] & 0x08000001,
        "imports": sorted(f"{s.name}@{s.version} {'weak' if s.bind == 2 else 'global'}" for s in img.dynsym[1:] if s.shndx == 0),
        ".preinit_array": arrays(img, ".preinit_array"),
        ".init_array": arrays(img, ".init_array"),
        ".fini_array": arrays(img, ".fini_array"),
        "load-time relocations": sorted(f"{k[0]} at {k[1]} to {k[2]} x{n}" for k, n in load_time.items()),
        "bound at load": sorted(bound),
        "FDEs": sorted(f"{start} for {length:#x}" for start, length in frames(img)),
    }


def compare_facts(ours: dict, lld: dict) -> list[str]:
    diffs = []
    for key in ours:
        a, b = ours[key], lld[key]
        if a == b:
            continue
        if isinstance(a, list) and isinstance(b, list) and key not in (".init_array", ".fini_array", ".preinit_array", "NEEDED"):
            only_a = sorted(set(a) - set(b))
            only_b = sorted(set(b) - set(a))
            diffs.append(f"{key}: only ours has {len(only_a)}: {only_a[:12]}; only lld's has {len(only_b)}: {only_b[:12]}")
        else:
            diffs.append(f"{key}: ours {a!r}, lld's {b!r}")
    return diffs


def compare_symbols(ours: Image, lld: Image) -> list[str]:
    """The functions and data each image has by name, and their sizes."""
    def named(img):
        return {s.name: s.size for s in img.syms if s.type in (1, 2) and img.count[s.name] == 1}
    a, b = named(ours), named(lld)
    diffs = []
    if only := sorted(set(a) - set(b)):
        diffs.append(f"symbols only ours has ({len(only)}): {only[:20]}")
    if only := sorted(set(b) - set(a)):
        diffs.append(f"symbols only lld's has ({len(only)}): {only[:20]}")
    if sized := sorted(n for n in set(a) & set(b) if a[n] != b[n]):
        diffs.append(f"symbols of another size ({len(sized)}): {[(n, a[n], b[n]) for n in sized[:12]]}")
    print(f"  {len(set(a) & set(b))} functions and objects by name in both", flush=True)
    return diffs


# --- The run ---------------------------------------------------------------------------------

async def main(given: Input) -> int:
    machine = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(os.uname().machine)
    if sys.platform != "linux" or machine is None:
        raise Failed(f"coldimage.py writes ELF images of x86-64 and AArch64 Linux, and this is {sys.platform} {os.uname().machine}")
    blaze = given.blaze or shutil.which("blaze") or ""
    if not os.path.isfile(blaze):
        raise Failed(f"no blaze {blaze or 'on PATH'}: INPUT's `blaze` names one")
    blaze = os.path.abspath(blaze)
    engine = os.path.join(os.path.dirname(blaze), "blaze-engine")
    if not os.path.isfile(engine):
        raise Failed(f"no blaze-engine beside {blaze}")
    work = os.path.abspath(given.work) if given.work else tempfile.mkdtemp(prefix="coldimage-")
    os.makedirs(work, exist_ok=True)
    try:
        return await compare(blaze, engine, work, machine)
    finally:
        if given.keep or given.work:
            print(f"kept {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)


async def compare(blaze: str, engine: str, work: str, machine: str) -> int:
    hello = os.path.join(work, "hello")
    os.makedirs(os.path.join(hello, "src"), exist_ok=True)
    with open(os.path.join(hello, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2024"\n')
    with open(os.path.join(hello, "src", "main.rs"), "w") as f:
        f.write(HELLO)
    shim = os.path.join(work, "shim")
    os.makedirs(shim, exist_ok=True)
    with open(os.path.join(shim, "cc"), "w") as f:
        f.write(SHIM.format(work=work))
    os.chmod(os.path.join(shim, "cc"), 0o755)
    env = dict(os.environ, PATH=shim + os.pathsep + os.environ.get("PATH", ""), CARGO_TERM_COLOR="never")
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    # A daemon started before this run has another PATH: one of this run's own.
    await run("blaze stop", [blaze, "stop"], cwd=hello, env=env, check=False)
    await run("blaze build of hello", [blaze, "build"], cwd=hello, env=env)
    line_file = os.path.join(work, "line.json")
    if not os.path.isfile(line_file):
        raise Failed(f"blaze built hello and the cc of {shim} saw no link with -pie and -nodefaultlibs: the link did not run the cc on PATH")
    with open(line_file) as f:
        line = json.load(f)
    cc, args = line["cc"], line["args"]
    lld_out, ours_out = os.path.join(work, "hello-lld"), os.path.join(work, "hello-patched")
    args = [lld_out if i and args[i - 1] == "-o" else a for i, a in enumerate(args)]
    if "-fuse-ld=lld" not in args:
        raise Failed(f"blaze's link line does not link with lld, the oracle: {shlex.join(args)}")
    _, _, said = await run("cc -###", [cc, "-###", *args], cwd=hello)
    request, checked = request_of(collect2_of(said.decode()), machine, ours_out)
    request_file = os.path.join(work, "request.json")
    with open(request_file, "w") as f:
        json.dump(request, f, indent=1)
    async with asyncio.TaskGroup() as group:
        group.create_task(run("the lld link", [cc, *args], cwd=hello))
        group.create_task(run("blaze-engine patch-image", [engine, "patch-image", request_file], cwd=hello))
    async with asyncio.TaskGroup() as group:
        runs = [group.create_task(run(f"running {p}", [p], cwd=work, secs=RUN_SECS, check=False)) for p in (ours_out, lld_out)]
    diffs = []
    (oc, oo, oe), (lc, lo, le) = runs[0].result(), runs[1].result()
    print(f"ran: ours exit {oc}, stdout {oo!r}; lld's exit {lc}, stdout {lo!r}", flush=True)
    if (oc, oo, oe) != (lc, lo, le):
        diffs.append(f"the runs differ: ours exit {oc}, stdout {oo!r}, stderr {oe[-400:]!r}; lld's exit {lc}, stdout {lo!r}, stderr {le[-400:]!r}")
    if lc != 3 or lo != b"hello, world\n":
        diffs.append(f"lld's hello does not print `hello, world` and exit 3: exit {lc}, {lo!r}")

    def read(path, label):
        with open(path, "rb") as f:
            return Image(label, Elf(path, f.read()))

    ours, lld = await asyncio.gather(asyncio.to_thread(read, ours_out, "ours"), asyncio.to_thread(read, lld_out, "lld's"))
    if "--fix-cortex-a53-843419" in checked and (patched := [s.name for s in lld.syms if s.name.startswith("__CortexA53843419_")]):
        diffs.append(f"lld patched {len(patched)} sequences for Cortex-A53 erratum 843419, which patch-image does not: {patched[:4]}")
    objects = []
    for item in request["inputs"]:
        if "path" in item:
            with open(item["path"], "rb") as f:
                objects.extend(Elf(n, d) for n, d in members(item["path"], f.read()))
    objects = [o for o in objects if o.type == 1]
    print(f"symbolic diff over {len(objects)} input objects:", flush=True)
    diffs += compare_symbols(ours, lld)
    diffs += compare_sections(objects, ours, lld)
    a, b = await asyncio.gather(asyncio.to_thread(facts, ours), asyncio.to_thread(facts, lld))
    diffs += compare_facts(a, b)
    for key, value in a.items():
        if isinstance(value, list):
            print(f"  {key}: {len(value)}", flush=True)
    if diffs:
        print(f"\n{len(diffs)} differences between the patched image and lld's:")
        for d in diffs:
            print(f"  - {d}")
        return 1
    print("the patched image and lld's run the same and reach the same")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main(typed(sys.argv[1] if len(sys.argv) > 1 else ""))))
    except* Failed as group:
        for e in group.exceptions:
            print(f"coldimage.py: {e}", file=sys.stderr)
        sys.exit(1)
