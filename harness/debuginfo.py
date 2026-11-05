#!/usr/bin/env python3
"""debuginfo.py: what a debugger and a backtrace are told of blaze's code.

Programs are built by `blaze build`, each in an empty home: the stress workspace and rung2 with a
panic put into a copy of each, a program of its own with panics behind inlined and `#[track_caller]`
functions, and a small program of its own that a daemon then follows through edits, built again
after each. Every one is run with RUST_BACKTRACE=1 and has to print,
for each panic, frames of the workspace's own files at the places the source says the panic passes
through: where it panics, and for the programs of its own every caller up to `main`, in order.
`llvm-dwarfdump --verify` has to find nothing wrong with any of the binaries, and gdb has to stop
at a line of a function blaze compiled, say where it is, and step to other lines of it. At a line
of a function of its own gdb has to print the parameters and locals as the source has them.

On Windows what a debugger reads is the PDB `link.exe /DEBUG` writes from the CodeView of the
objects, so `llvm-pdbutil` reads it in place of `llvm-dwarfdump`, and `cdb` is the debugger. Only
lines and procedures are written there: no variable is told of.

Then the cost: the time and the size of the stress workspace's program with and without debug
info, whether blaze leaves it out when the profile says `debug = false`, and whether a build that
asks for variables, in a home whose store a build of lines alone filled, has them all.

Its programs are of the dev profile, which v2 compiles with Cranelift.

--only NAME          one of: stress rung2 edits callers locals profile cost
"""
import bisect, os, re, shutil, struct, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.setdefault("CARGO_PROFILE_DEV_DEBUG", "true")
from dwarf import DWARFDUMP, OPTIMIZED_OUT, WINDOWS, Tally, dumped, held, own, own_ranges, panics, unit_of, units
from rebuild import Project, copy, scratch
from storelib import forget

PDBUTIL = shutil.which("llvm-pdbutil") or os.path.join(os.environ.get("LLVM_SYS_231_PREFIX", r"C:\llvm"), "bin", "llvm-pdbutil.exe")
CDB = shutil.which("cdb") or r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe"
DEBUGGER = "cdb" if WINDOWS else "gdb"
PANIC = 'if std::env::var_os("DEBUGINFO_PANIC").is_some() { drop(std::panic::take_hook()); panic!("deliberate"); }'

EDITS_TOML = """[package]
name = "sline"
version = "0.1.0"
edition = "2021"
"""
MAIN = "src/main.rs"
EDITS_MAIN = """use std::fmt::Display;

struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0.is_empty() {
            println!("never");
        }
    }
}

#[track_caller]
fn must(value: Option<u32>) -> u32 {
    match value {
        Some(it) => it,
        None => panic!("nothing there"),
    }
}

fn pick<T: Display>(items: &[T], at: usize) -> String {
    let _guard = Guard("pick");
    let found = items.get(at).map(|it| it.to_string());
    let length = found.map(|it| it.len() as u32);
    must(length).to_string()
}

fn walk(depth: u32) -> String {
    if depth > 0 {
        return walk(depth - 1) + "!";
    }
    let run = |at: usize| pick(&[1u8, 2, 3], at);
    run(7) + "?"
}

fn main() {
    println!("{}", walk(2));
}
"""
LOCALS_MAIN = """struct Point { x: i32, y: i64 }
struct Pair(u8, f64);

fn area(width: u32, scale: f32, name: &str) -> u32 {
    let point = Point { x: 3, y: -4 };
    let pair = Pair(7, 1.5);
    let list = [1u16, 2, 3];
    let tuple = (true, 'c', 9u64);
    let sum = width + point.x as u32 + pair.0 as u32 + list[1] as u32 + tuple.2 as u32;
    let borrowed = &point;
    println!("{} {} {} {}", sum, scale, name, borrowed.y);
    sum
}

fn main() {
    println!("{}", area(10, 2.5, "abc"));
}
"""
# What gdb is to print of each, stopped at the `println!` of `area`.
LOCALS = [("width", "10"), ("scale", "2.5"), ("name", '"abc"'), ("point", "sline::Point {x: 3, y: -4}"),
          ("pair", "sline::Pair (7, 1.5)"), ("list", "[1, 2, 3]"), ("tuple", "(true, 99 'c', 9)"), ("sum", "31"),
          ("*borrowed", "sline::Point {x: 3, y: -4}")]

# Panics behind functions rustc inlines even in a dev build, and behind `#[track_caller]` ones.
CALLERS_MAIN = """use std::panic::{catch_unwind, Location};

#[inline(always)]
fn always(items: &[u32], at: usize) -> u32 {
    items[at]
}

#[inline]
fn hinted(items: &[u32], at: usize) -> u32 {
    always(items, at) + 1
}

fn indexes() -> u32 {
    hinted(&[1, 2, 3], std::env::args().count() + 8)
}

#[track_caller]
fn inner(value: Option<u32>) -> u32 {
    value.expect("inner")
}

#[track_caller]
fn outer(value: Option<u32>) -> u32 {
    inner(value) + 1
}

fn tracked() -> u32 {
    outer(None) + 1
}

#[track_caller]
fn here() -> u32 {
    panic!("called from line {}", Location::caller().line())
}

fn located() -> u32 {
    here() + 1
}

trait Named {
    #[track_caller]
    fn must(&self) -> u32;
}

impl Named for u8 {
    fn must(&self) -> u32 {
        panic!("through a trait")
    }
}

fn dynamic() -> u32 {
    let it: &dyn Named = &3u8;
    it.must() + 1
}

fn closure() -> u32 {
    let take = |value: Option<u32>| value.unwrap() + 1;
    take(None) + 1
}

fn bound() -> u32 {
    let items = [1u32, 2, 3];
    let item = items[std::env::args().count() + 8];
    item + 1
}

fn main() {
    let cases: [fn() -> u32; 6] = [indexes, tracked, located, dynamic, closure, bound];
    for case in cases {
        let _ = catch_unwind(case);
    }
}
"""

# (what the step is, [(old, new)])
EDITS = [
    ("nothing edited", []),
    ("a line added above everything", [("use std::fmt::Display;", "// One line further down.\nuse std::fmt::Display;")]),
    ("the function that panics made longer", [("        None => panic!", "        Some(0) => 1,\n        None => panic!")]),
    ("its caller, with the call on another line", [("    must(length).to_string()", '    let found = must(length);\n    found.to_string()')]),
    ("a function added between them", [
        ("fn walk(", "fn between(at: usize) -> String {\n    pick(&[1u8, 2, 3], at) + \".\"\n}\n\nfn walk("),
        ("let run = |at: usize| pick(&[1u8, 2, 3], at);", "let run = |at: usize| between(at);"),
    ]),
    ("lines taken away again", [("// One line further down.\n", "")]),
]


def line_of(text, needle):
    """The line of `text` that `needle` is on, counted from 1."""
    assert text.count(needle) == 1, needle
    return text[:text.index(needle)].count("\n") + 1


def backtraces(binary, cwd, env=None):
    ran = subprocess.run([binary], capture_output=True, text=True, errors="replace", cwd=cwd, timeout=900,
                         env={**os.environ, "RUST_BACKTRACE": "1", **(env or {})})
    return panics(ran.stderr)


def sline(what, main):
    """A crate `sline` of its own with `main` as its `src/main.rs`: (the run's directory, the crate)."""
    work = scratch(f"blaze-debuginfo-{what}-")
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, main)):
        open(os.path.join(ws, path), "w").write(text)
    return work, ws


def build(ws, package, home, env=None):
    """`blaze build -p PACKAGE` of `ws` with the home `home` (an empty one unless a build was
    there before) and `env`: the program, refused with what blaze said when it is not built."""
    project = Project(ws, os.path.dirname(home), home=home, env=env, name=os.path.basename(home))
    try:
        built = project.build(package, "-p", package)
    finally:
        project.stop()
    if built.status != 0 or not built.executable:
        raise SystemExit(f"blaze build -p {package} in {ws} failed ({built.status}):\n{built.said[-3000:]}")
    return built.executable


class Image:
    """What the headers of a PE executable say of it."""

    def __init__(self, binary):
        data = self.data = open(binary, "rb").read()
        pe, = struct.unpack_from("<I", data, 0x3c)
        count, = struct.unpack_from("<H", data, pe + 6)
        optional_size, = struct.unpack_from("<H", data, pe + 20)
        self.optional = pe + 24
        self.entry, = struct.unpack_from("<I", data, self.optional + 16)
        headers = [self.optional + optional_size + 40 * i for i in range(count)]
        # (address, bytes of it in the file, where they are in the file)
        self.sections = [struct.unpack_from("<III", data, at + 12) for at in headers]

    def directory(self, index):
        return struct.unpack_from("<II", self.data, self.optional + 112 + 8 * index)

    def at(self, address):
        return next(raw + address - start for start, size, raw in self.sections if start <= address < start + size)

    def address(self, section, offset):
        return self.sections[section - 1][0] + offset

    def frames(self):
        """Where the code of each frame an unwinder finds starts and ends."""
        address, size = self.directory(3)
        table = self.at(address)
        return {begin: end for begin, end, _ in struct.iter_unpack("<III", self.data[table:table + size])}

    def pdb(self):
        """The identity of the PDB the linker wrote for it, and what it is called."""
        address, size = self.directory(6)
        for at in range(self.at(address), self.at(address) + size, 28):
            kind, _, _, raw = struct.unpack_from("<IIII", self.data, at + 12)
            if kind == 2 and self.data[raw:raw + 4] == b"RSDS":
                a, b, c = struct.unpack_from("<IHH", self.data, raw + 4)
                rest = self.data[raw + 12:raw + 20].hex().upper()
                name = self.data[raw + 24:self.data.index(b"\0", raw + 24)].decode()
                return f"{a:08X}-{b:04X}-{c:04X}-{rest[:4]}-{rest[4:]}", name
        return None, None


class Pdb:
    """What the PDB beside an executable tells a debugger of blaze's objects: the procedures
    as (address, end, name), the lines as (file, line, address), and how many variables."""
    read = {}

    @classmethod
    def of(cls, binary):
        key = (binary, os.path.getmtime(binary))
        if key not in cls.read:
            cls.read[key] = cls(binary)
        return cls.read[key]

    def __init__(self, binary):
        self.image = image = Image(binary)
        guid, name = image.pdb()
        path = os.path.join(os.path.dirname(binary), os.path.basename(name or "none.pdb"))
        ran = subprocess.run([PDBUTIL, "dump", "--summary", "--symbols", "-l", path], capture_output=True, text=True, errors="replace")
        self.sound = ran.returncode == 0
        found = re.search(r"GUID: \{([0-9A-Fa-f-]+)\}", ran.stdout)
        self.same = bool(guid and found and found.group(1).upper() == guid)
        # It prints the lines of every module, then the symbols of every module.
        lines, _, symbols = ran.stdout.partition("\n                          Symbols")
        ours = set()
        self.procedures, self.lines, self.variables = [], [], 0
        for module in re.split(r"^\s*Mod (?=\d{4} \|)", symbols, flags=re.M)[1:]:
            if not re.search(r"Ver = blaze\b", module):
                continue
            ours.add(module[:4])
            self.variables += len(re.findall(r"\| S_(?:LOCAL|REGREL32|BPREL32|REGISTER) ", module))
            for name, section, offset, size in re.findall(
                    r"S_[GL]PROC32(?:_ID)? \[size = \d+\] `(.*)`\n.*addr = (\d+):(\d+), code size = (\d+)", module):
                begin = image.address(int(section), int(offset))
                self.procedures.append((begin, begin + int(size), name))
        self.modules = len(ours)
        for module in re.split(r"^\s*Mod (?=\d{4} \|)", lines, flags=re.M)[1:]:
            if module[:4] not in ours:
                continue
            file = section = None
            for line in module.splitlines()[1:]:
                if block := re.match(r"\s+(\d{4}):[0-9A-F]{8}-[0-9A-F]{8}, line/addr", line):
                    section = int(block.group(1))
                elif line and not line[0].isspace():
                    file = line.rsplit(" (", 1)[0]
                else:
                    self.lines += [(file, int(n), image.address(section, int(at, 16)))
                                   for n, at in re.findall(r"(\d+) ([0-9A-F]{8})\b", line)]

    def function(self, address):
        return next((name for begin, end, name in self.procedures if begin <= address < end), None)


def verify(tally, what, binary):
    """Whether `llvm-dwarfdump --verify` finds the program sound."""
    if WINDOWS:
        pdb = Pdb.of(binary)
        verdict = "INVALID" if not pdb.sound else "NOT ITS PDB" if not pdb.same else "sound" if pdb.modules else "NO UNIT"
        return tally.say(f"{what}: llvm-pdbutil reads the PDB", verdict, f"{pdb.modules} modules of blaze's")
    read = subprocess.run([DWARFDUMP, "--verify", binary], capture_output=True, text=True)
    ours = sum(bool(re.search(r'DW_AT_producer\s+\("blaze[ "]', unit)) for unit in units(binary))
    wrong = read.returncode != 0 or "No errors." not in read.stdout
    said = next((l for l in read.stdout.splitlines() if "error" in l.lower()), "")
    verdict = "INVALID" if wrong else "sound" if ours else "NO UNIT"
    tally.say(f"{what}: llvm-dwarfdump --verify", verdict, said[:80] or f"{ours} units of blaze's")


def frames_of(binary):
    """Where the code of each frame an unwinder finds starts and ends: the ones in the table
    the `PT_GNU_EH_FRAME` program header leads to. A splice adds frames to that table that
    no section of the file is said to hold."""
    data = open(binary, "rb").read()
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    headers = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize) for i in range(phnum)]
    loads = [(vaddr, offset, filesz) for kind, _, offset, vaddr, _, filesz, _, _ in headers if kind == 1]

    def at(address):
        return next(offset + address - vaddr for vaddr, offset, filesz in loads if vaddr <= address < vaddr + filesz)

    def pointer(encoding, address):
        """The address written at `address` as `encoding` says, and how many bytes it takes."""
        size, form = {0x00: (8, "<Q"), 0x03: (4, "<I"), 0x04: (8, "<Q"), 0x0b: (4, "<i"), 0x0c: (8, "<q")}[encoding & 0x0f]
        value, = struct.unpack_from(form, data, at(address))
        return value + (address if encoding & 0x70 == 0x10 else 0), size

    def code_encoding(common):
        """How the frames of the common entry at `common` write where their code starts."""
        rest = at(common) + 9
        end = data.index(b"\0", rest)
        letters, rest = data[rest:end], end + 1
        for _ in range(4):
            while data[rest] & 0x80:
                rest += 1
            rest += 1
        for letter in letters[1:]:
            if letter == ord("R"):
                return data[rest]
            if letter == ord("L"):
                rest += 1
            elif letter == ord("P"):
                rest += 1 + pointer(data[rest], 0)[1]
        return 0

    table = next((vaddr for kind, _, _, vaddr, _, _, _, _ in headers if kind == 0x6474e550), None)
    if table is None:
        return {}
    _, frame_encoding, count_encoding, table_encoding = data[at(table):at(table) + 4]
    _, size = pointer(frame_encoding, table + 4)
    count, counted = pointer(count_encoding, table + 4 + size)
    assert table_encoding == 0x3b, f"a search table written as {table_encoding:#x}"
    ends = {}
    for i in range(count):
        _, frame = struct.unpack_from("<ii", data, at(table + 4 + size + counted + i * 8))
        frame += table
        encoding = code_encoding(frame + 4 - struct.unpack_from("<I", data, at(frame + 4))[0])
        start, size_of = pointer(encoding, frame + 8)
        length, _ = pointer(encoding & 0x0f, frame + 8 + size_of)
        ends[start] = start + length
    return ends


def unlined(binary):
    """The runs of code in blaze's functions that no row of the line table gives a line.

    A row is of the code from its address to the next row's in its sequence, and a row of line 0
    gives none. A function is looked at as far as the unwinder has it: past that it is filled, and
    nothing runs there. One the unwinder does not have and with no row at all is assembly alone,
    which `unwinding` counts."""
    rows = subprocess.run([DWARFDUMP, "--debug-line", binary], capture_output=True, text=True).stdout
    lined, last = [], None
    for address, line, flags in re.findall(r"^0x([0-9a-f]{16}) +(\d+)(?: +\d+)+(.*)$", rows, re.M):
        address = int(address, 16)
        if last and last[1] and address > last[0]:
            lined.append((last[0], address))
        last = None if "end_sequence" in flags else (address, int(line))
    lined.sort()
    starts, ends, gaps = [a for a, _ in lined], frames_of(binary), []
    for a, b in sorted(own_ranges(binary)):
        b, at = min(b, ends.get(a, b)), a
        i, any_row, holes = max(bisect.bisect_right(starts, a) - 1, 0), False, []
        while i < len(lined) and lined[i][0] < b:
            x, y = lined[i]
            if y > at:
                any_row = True
                if x > at:
                    holes.append((at, x))
                at = y
            i += 1
        if at < b:
            holes.append((at, b))
        if any_row or a in ends:
            gaps += holes
    return gaps


def unwinding(tally, what, binary):
    """Whether the functions a debugger is told of are the ones an unwinder is told of.

    Every run of code in a unit of blaze's has to begin where a frame of the unwinder's begins and
    end no earlier than it. It may end later: a function shorter than a jump to another place is
    filled up to the length of one, for an edit to put that jump there, and the fill is not code
    to unwind from. A `#[naked]` function is its assembly, which says nothing of unwinding and has
    no lines either: one with no entry and no line is that.

    On Windows the frames are the entries of `.pdata` and the functions are the procedures of
    blaze's modules in the PDB."""
    if WINDOWS:
        pdb = Pdb.of(binary)
        ours, ends = {(begin, end) for begin, end, _ in pdb.procedures}, pdb.image.frames()
        bare = sorted((a, b) for a, b in ours if a not in ends)
        lined = sorted({address for _, _, address in pdb.lines})
        return tally.say(f"{what}: .pdata and the procedures", *frames_agree(ours, ends, bare, lined))
    ours = own_ranges(binary)
    ends = frames_of(binary)
    bare = sorted((a, b) for a, b in ours if a not in ends)
    rows = subprocess.run([DWARFDUMP, "--debug-line", binary], capture_output=True, text=True).stdout if bare else ""
    # The row a sequence ends with is of the address past its code, where another function may begin.
    lined = sorted({int(a, 16) for a in re.findall(r"^0x([0-9a-f]{16}) +[1-9](?!.*end_sequence)", rows, re.M)})
    tally.say(f"{what}: .eh_frame and the ranges", *frames_agree(ours, ends, bare, lined))
    gaps = unlined(binary)
    worst = ", ".join(f"{a:#x}+{b - a}" for a, b in sorted(gaps, key=lambda gap: gap[0] - gap[1])[:3])
    tally.say(f"{what}: a line for every instruction", "NO LINE" if gaps or not ours else "all",
              f"{len(gaps)} runs of {sum(b - a for a, b in gaps)} bytes without one in {len(ours)} functions" + (f": {worst}" if gaps else ""))


def frames_agree(ours, ends, bare, lined):
    """The verdict on the functions `ours` against the frames `ends`, where `bare` are the
    ones no frame begins at and `lined` is where the lines are."""
    without = sum(any(a <= row < b for row in lined) for a, b in bare)
    beyond = sum(a in ends and ends[a] > b for a, b in ours)
    filled = sum(a in ends and ends[a] < b for a, b in ours)
    verdict = "NO FUNCTIONS" if not ours else "DISAGREE" if without or beyond else "agree"
    return verdict, f"{len(ours)} functions, {without} without an entry, {beyond} that end before theirs, {filled} filled past theirs, {len(bare) - without} of assembly alone"


def libdw_shift(binary):
    """How far from a frame's address perf's libdw unwinder looks up that frame's CFI in `binary`.
    perf (tools/perf/util/unwind-libdw.c, __report_module, up to 6.17 at least) reports the module at
    the start of the mapping of the frame's address less that mapping's file offset. That is where the
    file is loaded only where the executable segment's address and file offset are on the same page,
    and lld lays the segment out a page above its offset. Then every lookup lands that far from the
    frame, in whatever code is there, and an unwinding stops where that code's rules differ."""
    with open(binary, "rb") as f:
        data = f.read(1 << 16)
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    headers = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize) for i in range(phnum)]
    loads = [(flags, offset, vaddr) for kind, flags, offset, vaddr, *_ in headers if kind == 1]
    page = os.sysconf("SC_PAGE_SIZE")
    first = min(vaddr for _, _, vaddr in loads) & -page
    return next(((vaddr & -page) - (offset & -page) - first for flags, offset, vaddr in loads if flags & 1), 0)


def naked_functions(root):
    """The names of the `#[unsafe(naked)]` functions of the sources under `root`."""
    names = set()
    for directory, subdirectories, files in os.walk(root):
        subdirectories[:] = [it for it in subdirectories if it != "target"]
        for file in files:
            if file.endswith(".rs"):
                text = open(os.path.join(directory, file), encoding="utf-8", errors="replace").read()
                names.update(re.findall(r"#\[unsafe\(naked\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?extern\s+\"[^\"]*\"\s+fn\s+(\w+)", text))
    return names


def profiler(tally, what, binary, cwd, crates):
    """Whether perf, unwinding by `.eh_frame`, names the functions of `crates` and their lines."""
    if WINDOWS:
        return tally.say(f"{what}: perf", "not run", "perf is of Linux")
    perf = shutil.which("perf")
    data = os.path.join(tempfile.mkdtemp(prefix="blaze-debuginfo-perf-"), "perf.data")
    # One run is over in milliseconds, a few dozen samples: whether any lands in blaze's functions, or
    # unwinds back to main, was chance. Twenty runs under one recording give hundreds.
    again = 'for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do "$0" >/dev/null || exit; done'
    record = perf and subprocess.run([perf, "record", "-o", data, "-F", "5000", "--call-graph", "dwarf", "--", "sh", "-c", again, binary],
                                     cwd=cwd, capture_output=True, text=True)
    if not perf or record.returncode != 0 or not os.path.exists(data):
        # Not a verdict on the binary: the machine does not let this user profile.
        tally.say(f"{what}: perf", "not run", "no perf here, or it may not sample: see kernel.perf_event_paranoid")
        return
    script = subprocess.run([perf, "script", "-i", data, "-F", "ip,sym,srcline"], capture_output=True, text=True).stdout
    stacks = [stack for stack in script.split("\n\n") if stack.strip()]
    shift = libdw_shift(binary)
    ours = lambda frames: [(name, line) for name, line in frames if any(f"{len(c)}{c}" in name or f"{c}::" in name for c in crates)]
    # A frame's name may have spaces (`<T as Trait>::f`), and a function inlined at an address has
    # its frame there first, its line marked "(inlined)".
    by_stack = [re.findall(r"^\s+[0-9a-f]+ (.+)\n\s+(\S+)(?: \(inlined\))?$", stack, re.M) for stack in stacks]
    mine = ours(frame for frames in by_stack for frame in frames)
    # Where perf looks a frame's CFI up somewhere else, the frames it unwinds to are wherever that
    # lands, a static as well as code: only the frame sampled is read from the program as it is.
    # A line 0 of one of rustc's codegen units is rustc saying of its own code that it has no
    # line: the crate's functions that were left to rustc are not what is asked about here.
    read = ours(frames[0] for frames in by_stack if frames) if shift else mine
    # A naked function is its assembly alone, which rustc gives no line either (nor a subprogram).
    naked = naked_functions(cwd)
    lost = [name for name, line in read
            if not re.search(r"\.rs:\d+$|-cgu\.\d+:0$", line) and name.rsplit("::", 1)[-1] not in naked]
    # Where the program's `main` is inlined into the `main` that calls it, its frame is named
    # `main` and its line is one of the program's; the shell's `main` has no such line.
    through = sum(bool(re.search(r"4main\b|::main\b|^\s+[0-9a-f]+ main\n\s+\S+\.rs:\d+", stack, re.M)) for stack in stacks)
    # Where perf looks a frame's CFI up somewhere else, how far its stacks get says nothing of the CFI:
    # some reach main where the code it lands in has the same rules, and some stop.
    verdict = ("NO SAMPLES" if not mine else "NO LINE" if lost else "named" if through
               else "not unwound by perf" if shift else "NOT UNWOUND")
    tally.say(f"{what}: perf, unwinding by .eh_frame", verdict,
              f"{len(mine)} frames of blaze's functions in {len(stacks)} samples, {len(lost)} without a line, {through} samples back to main"
              + (f": {lost[0][:60]}" if lost else "")
              + (f"; perf's libdw unwinder looks CFI up {shift:#x} from each frame in this layout" if shift and not through else ""))
    if mine and not through and not shift:
        # Where the stacks with the crate's functions in them end tells what the unwinding stopped at.
        ours = [stack for stack in stacks if any(f"{len(c)}{c}" in stack or f"{c}::" in stack for c in crates)]
        for stack in ours[:3]:
            print("\n".join("    perf: " + it.strip()[:160] for it in stack.strip().splitlines()[-12:]) + "\n    perf: --")


def debugger(tally, what, binary, cwd, place, env=None):
    """Has gdb stop at `place` (file:line), say where that is, and step on from there."""
    if WINDOWS:
        return cdb(tally, what, binary, cwd, place, env)
    commands = ["set pagination off", "set confirm off", "set print demangle off", "set print asm-demangle off",
                "set startup-with-shell off", f"break {place}", "run", "echo <where>\\n", "bt 3", "echo <mapped>\\n",
                "info proc mappings", "print/x $pc", "echo <line>\\n", "info line *$pc",
                *[x for _ in range(4) for x in ("echo <step>\\n", "step", "frame")], "kill"]
    for name, value in (env or {}).items():
        commands.insert(5, f"set environment {name} {value}")
    args = [x for c in commands for x in ("-ex", c)]
    try:
        ran = subprocess.run(["gdb", "-q", "-nx", "-batch", *args, binary], capture_output=True, text=True,
                             errors="replace", cwd=cwd, timeout=300)
    except subprocess.TimeoutExpired:
        return tally.say(f"{what}: gdb at {place}", "NO ANSWER")
    out = ran.stdout
    file, line = place.rsplit(":", 1)
    said = f"{what}: gdb at {os.path.basename(file)}:{line}"
    # A newer gdb numbers which of a breakpoint's places it stopped at: "Breakpoint 1.1,".
    stop = re.search(r"Breakpoint 1(?:\.\d+)?, .*? at (\S+):(\d+)", out, re.S)
    if not stop or stop.group(2) != line or not file.endswith(os.path.basename(stop.group(1))):
        # What gdb said up to where it would have stopped is what tells why it did not.
        print("\n".join("    gdb: " + it[:200] for it in (out.split("<where>")[0] + ran.stderr).strip().splitlines()[-12:]))
        return tally.say(said, "NOT STOPPED", (out.strip().splitlines() or ["nothing printed"])[-1][:80])
    value = re.search(r"\$1 = (0x[0-9a-f]+)", out)
    base = re.search(r"^\s*(0x[0-9a-f]+)\s+0x[0-9a-f]+\s+0x[0-9a-f]+\s+0x0\s", out.split("<mapped>")[1], re.M)
    if not value or not base:
        return tally.say(said, "NO ADDRESS")
    # The file's address of the code, whether or not the program is loaded where it says.
    pc, base = int(value.group(1), 16), int(base.group(1), 16)
    producers = [unit_of(binary, address) for address in (pc - base, pc)]
    if not any(it.startswith("blaze") for it in producers):
        return tally.say(said, "NOT BLAZE'S", f"the code is of {producers[0]}")
    name = re.search(r"^#0\s+(.*?) \(", out.split("<where>")[1], re.M)
    if f"Line {line} of" not in out.split("<line>")[1]:
        return tally.say(said, "NO LINE", out.split("<line>")[1].strip()[:80])
    stepped = {m for part in out.split("<step>")[1:] for m in re.findall(r"^#0 .*? at (\S+:\d+)", part, re.M)}
    stepped = {s for s in stepped if not s.endswith(f":{line}")}
    if not stepped:
        # What gdb said from the line on is what tells where the steps went.
        print("\n".join("    gdb: " + it[:200] for it in (out.split("<line>")[1] + ran.stderr).strip().splitlines()[-24:]))
        return tally.say(said, "NOT STEPPED")
    frames = len(re.findall(r"^#\d+ ", out.split("<where>")[1].split("<mapped>")[0], re.M))
    tally.say(said, "stopped", f"in {name.group(1)[:40] if name else '?'}, {frames} frames up, stepped to {', '.join(sorted(stepped))[:60]}")


def cdb(tally, what, binary, cwd, place, env=None):
    """The same of cdb. It names a line of several places as ambiguous and stops at none of
    them, and rustc's own copy of the code is in the program too, so it is asked which places
    the line is, and then to stop at the ones of them the PDB says are blaze's."""
    file, line = place.rsplit(":", 1)
    said = f"{what}: cdb at {os.path.basename(file)}:{line}"
    pdb = Pdb.of(binary)
    file = file.replace("/", "\\")
    ours = sorted({address for path, n, address in pdb.lines if n == int(line) and path.endswith("\\" + file)})
    if not ours:
        return tally.say(said, "NOT BLAZE'S", "no line of a module of blaze's is that one")
    # The program is loaded where it likes: places are counted from where it starts.
    base = f"@$exentry-{pdb.image.entry:#x}"
    commands = ["l+t", ".echo <break>", f"bu `{file}:{line}`", "bl", "bc *", *[f"bp {base}+{address:#x}" for address in ours],
                "g", ".echo <where>", "kn 4", *[x for _ in range(4) for x in (".echo <step>", "t", "kn 1")], "q"]
    try:
        ran = subprocess.run([CDB, "-lines", "-y", os.path.dirname(binary), binary], input="\n".join(commands) + "\n",
                             capture_output=True, text=True, errors="replace", cwd=cwd, timeout=300,
                             env={**os.environ, **(env or {})})
    except subprocess.TimeoutExpired:
        return tally.say(said, "NO ANSWER")
    out = ran.stdout
    if "<where>" not in out or "<break>" not in out:
        return tally.say(said, "NO ANSWER", (out.strip().splitlines() or ["nothing printed"])[-1][:80])
    loaded = re.search(r"^ModLoad: ([0-9a-f`]+) ", out, re.M)
    start = int(loaded.group(1).replace("`", ""), 16)
    asked = out.split("<break>")[1].split("<where>")[0]
    by_line = {int(a.replace("`", ""), 16) - start for a in re.findall(r"\b([0-9a-f]{8}`[0-9a-f]{8})\b", asked.split("Breakpoint ")[0])}
    if not by_line & set(ours):
        return tally.say(said, "NOT BY LINE", f"cdb finds the line at {len(by_line)} places, none of them blaze's")
    where = out.split("<where>")[1].split("<step>")[0]
    stop = re.search(r"^00 \S+ \S+\s+(\S.*?)(?:\+0x[0-9a-f]+)? \[(.*) @ (\d+)\]", where, re.M)
    if "Breakpoint" not in asked or not stop or stop.group(3) != line or not stop.group(2).endswith(file):
        return tally.say(said, "NOT STOPPED", (where.strip().splitlines() or ["nothing printed"])[-1][:80])
    stepped = {f"{os.path.basename(path.replace(chr(92), '/'))}:{n}" for part in out.split("<step>")[1:]
               for path, n in re.findall(r"^00 .* \[(.*) @ (\d+)\]", part, re.M)}
    stepped = {s for s in stepped if not s.endswith(f":{line}")}
    if not stepped:
        return tally.say(said, "NOT STEPPED")
    frames = len(re.findall(r"^[0-9a-f]{2} [0-9a-f`]+ [0-9a-f`]+ ", where, re.M))
    tally.say(said, "stopped", f"in {stop.group(1).split('!')[-1][:40]}, {frames} frames up, stepped to {', '.join(sorted(stepped))[:60]}")


def call_site(ours):
    """Where the function that panics first is called from, for a debugger to stop at and step
    into it from: the panic itself is the last line that runs of the function it is in."""
    for _, frames in ours:
        if places := own(frames):
            return places[min(1, len(places) - 1)][1]


def program(tally, what, workspace, crates, package, inject):
    """A copy of the workspace with a panic put in by `inject`, (file, old, new): its one panic
    has to have a frame at the line of `new` the panic is on."""
    work = scratch(f"blaze-debuginfo-{what}-")
    ws = copy(workspace, os.path.join(work, "ws"))
    path, old, new = inject
    text = open(os.path.join(ws, path)).read()
    assert old in text, old
    text = text.replace(old, new, 1)
    open(os.path.join(ws, path), "w").write(text)
    env = {"DEBUGINFO_PANIC": "1"}
    binary = build(ws, package, os.path.join(work, "home"))
    ours = backtraces(binary, ws, env)
    site = f"{path}:{line_of(text, PANIC)}"
    held(tally, f"{what}: frames of each panic", ours, [[(None, site)]])
    verify(tally, what, binary)
    unwinding(tally, what, binary)
    if place := call_site(ours):
        debugger(tally, what, binary, ws, place[2:], env)
    else:
        tally.say(f"{what}: {DEBUGGER}", "NO PLACE")
    functions, count = described(binary)
    tally.say(f"{what}: what a debugger is told of", "counted", f"{functions} functions, {count} parameters and locals in them")
    profiler(tally, what, binary, ws, crates + [package])


def edits(tally, apart=False):
    """The program of its own, a step at a time under a daemon. With `apart`, the profile has
    what a debugger reads kept beside the objects, and the edits go into a program built so."""
    if apart:
        os.environ["CARGO_PROFILE_DEV_SPLIT_DEBUGINFO"] = "unpacked"
        try:
            return edited(tally, "edits, kept apart")
        finally:
            del os.environ["CARGO_PROFILE_DEV_SPLIT_DEBUGINFO"]
    edited(tally, "edits")


def edit_sites(text):
    """Where the one panic of the program of EDITS_MAIN, as `text` has it, passes through: `must`
    panics, called by `pick`, called (through `between` once it is there) by the closure of
    `walk`, called by `walk` at the bottom of its recursion of three, called by `main`."""
    at = lambda needle: f"{MAIN}:{line_of(text, needle)}"
    sites = [("sline::must", at("None => panic!")), ("sline::pick", at("must(length)"))]
    if "fn between(" in text:
        sites.append(("sline::between", at('pick(&[1u8, 2, 3], at) + "."')))
    sites += [(None, at("let run = |at: usize|")), ("sline::walk", at("run(7)"))]
    sites += [("sline::walk", at("return walk(depth - 1)"))] * 2
    return [sites + [("sline::main", at("walk(2)"))]]


def edited(tally, part):
    work, ws = sline("edits", EDITS_MAIN)
    project = Project(ws, work)
    binary = None
    try:
        for what, changes in EDITS:
            text = open(os.path.join(ws, MAIN)).read()
            for old, new in changes:
                assert old in text, old
                text = text.replace(old, new, 1)
            open(os.path.join(ws, MAIN), "w").write(text)
            built = project.build("sline", "-p", "sline")
            if project.daemon is None or not project.daemon.alive():
                raise SystemExit(f"the daemon died at {what!r}: its log is in {project.home}")
            if built.status != 0 or not built.executable:
                tally.say(f"{part}: {what}", "REFUSED", built.said.strip()[-60:])
                continue
            binary = built.executable
            ours, sites = backtraces(binary, ws), edit_sites(text)
            held(tally, f"{part}: {what}", ours, sites)
            verify(tally, f"{part}: {what}", binary)
            unwinding(tally, f"{part}: {what}", binary)
    finally:
        project.stop()
    if binary is None:
        return tally.say(f"{part}: the last binary", "NONE BUILT")
    ours = backtraces(binary, ws)
    if place := call_site(ours):
        debugger(tally, f"{part}: the last binary", binary, ws, place[2:])
    if not WINDOWS:
        # `walk` asks for the item at 7, through `between` or not.
        line = line_of(open(os.path.join(ws, MAIN)).read(), "must(length)")
        mine = printed_by("gdb", binary, ws, f"{MAIN}:{line}", ["at"])["at"]
        right = mine == "7"
        tally.say(f"{part}: the last binary: gdb prints an argument", "as it is" if right else "NOT AS IT IS", f"at = {mine}")
    if os.environ.get("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO"):
        kept = sum("DW_AT_GNU_dwo_name" in unit and bool(re.search(r'DW_AT_producer\s+\("blaze[ "]', unit)) for unit in units(binary))
        tally.say(f"{part}: units of the last binary kept apart", "some" if kept else "NONE", str(kept))


def described(binary):
    """How many functions blaze's units tell a debugger of, and how many variables in them."""
    if WINDOWS:
        pdb = Pdb.of(binary)
        return len(pdb.procedures), pdb.variables
    functions = variables = 0
    for unit in dumped(binary, "--debug-info").split("DW_TAG_compile_unit")[1:]:
        if re.search(r'DW_AT_producer\s+\("blaze', unit):
            functions += unit.count("DW_TAG_subprogram")
            variables += unit.count("DW_TAG_variable") + unit.count("DW_TAG_formal_parameter")
    return functions, variables


def variables(tally):
    """Whether gdb prints the parameters and locals of a function of blaze's as they are.

    A value the optimizer did away with is to be said to be so, and is not counted as wrong."""
    work, ws = sline("locals", LOCALS_MAIN)
    binary = build(ws, "sline", os.path.join(work, "home"))
    verify(tally, "locals", binary)
    if WINDOWS:
        functions, count = described(binary)
        return tally.say("locals: cdb prints them", "GUESSED" if count else "not told of",
                         f"{count} described in {functions} functions: no types are written to `.debug$T` yet")
    line = LOCALS_MAIN[:LOCALS_MAIN.index("println!")].count("\n") + 1
    commands = ["set pagination off", "set startup-with-shell off", f"break {MAIN}:{line}", "run",
                *[x for name, _ in LOCALS for x in (f"echo <{name}>\\n", f"print {name}")], "kill"]
    ran = subprocess.run(["gdb", "-q", "-nx", "-batch", *[x for c in commands for x in ("-ex", c)], binary],
                         capture_output=True, text=True, errors="replace", cwd=ws, timeout=300)
    printed = {name: re.search(rf"<{re.escape(name)}>\n\$\d+ = (.*)", ran.stdout) for name, _ in LOCALS}
    right = [name for name, value in LOCALS if printed[name] and printed[name].group(1) == value]
    functions, count = described(binary)
    gone = [name for name, _ in LOCALS if printed[name] and OPTIMIZED_OUT in printed[name].group(1)]
    wrong = [f"{name} = {printed[name].group(1) if printed[name] else 'unknown'}" for name, _ in LOCALS
             if name not in right and name not in gone]
    said = f"; optimized out: {', '.join(gone)[:200]}" if gone else ""
    tally.say("locals: gdb prints them", "NOT AS THEY ARE" if wrong else "printed",
              f"{len(right)} of {len(LOCALS)}, {len(gone)} optimized out, {count} described in {functions} functions"
              + (f", but {wrong[0][:50]}" if wrong else "") + said)


def caller_sites():
    """Where each panic of CALLERS_MAIN passes through, from where it panics to `main`'s
    `catch_unwind`, in the order `main` runs the cases."""
    at = lambda needle: f"{MAIN}:{line_of(CALLERS_MAIN, needle)}"
    main = ("sline::main", at("catch_unwind(case)"))
    return [
        [("sline::always", at("items[at]")), ("sline::hinted", at("always(items, at) + 1")), ("sline::indexes", at("hinted(&[1, 2, 3]")), main],
        [("sline::inner", at('value.expect("inner")')), ("sline::outer", at("inner(value) + 1")), ("sline::tracked", at("outer(None) + 1")), main],
        [("sline::here", at('panic!("called from line')), ("sline::located", at("here() + 1")), main],
        [(None, at('panic!("through a trait")')), ("sline::dynamic", at("it.must() + 1")), main],
        [(None, at("value.unwrap() + 1")), ("sline::closure", at("take(None) + 1")), main],
        [("sline::bound", at("let item = items[")), main],
    ]


def callers(tally):
    """Whether panics behind inlined and `#[track_caller]` functions have a frame at every
    place the source says they pass through."""
    work, ws = sline("callers", CALLERS_MAIN)
    binary = build(ws, "sline", os.path.join(work, "home"))
    held(tally, "callers: frames of each panic", backtraces(binary, ws), caller_sites())
    verify(tally, "callers", binary)


def printed_by(debugger, binary, cwd, place, names):
    """What a debugger prints for each of the names when stopped at a place, addresses aside.

    gdb prints `<optimized out>` for a value it has no location for, but not always: for an
    enum, whose variant it works out from the value, it fails to read memory at address 0, and
    the pretty-printer of a `&str` fails with "value has been optimized out" and leaves the
    value blank. rustc's optimized builds get the same. Where gdb printed no value and said
    the value is optimized out, or `info address` says the symbol is, the name is optimized out.
    A value of which only a part has a location is printed up to the part that has none: a
    slice whose length is kept and whose pointer is not is `&[i32](size=2)`, and the printer
    of its elements fails with "value has been optimized out", or gdb says `<error reading
    variable: ...>` in its place. Such a value is what was printed, then ` <optimized out>`."""
    def mark(name):
        """A line that starts what is said of the name, on both of gdb's streams."""
        return (f"echo @@{name}@@\\n", f"python gdb.write('@@{name}@@\\n', gdb.STDERR)")
    commands = ["set pagination off", "set startup-with-shell off", f"break {place}", "run",
                *[x for name in names for x in (*mark(name), f"print {name}", f"info address {name}")], *mark(""), "kill"]
    ran = subprocess.run([debugger, "-q", "-batch", *[x for c in commands for x in ("-ex", c)], binary],
                         capture_output=True, text=True, errors="replace", cwd=cwd, timeout=300)

    def said(stream, name):
        found = re.search(rf"^@@{re.escape(name)}@@\n(.*?)^@@", stream, re.S | re.M)
        return found.group(1) if found else ""
    printed = {}
    for name in names:
        out = said(ran.stdout, name)
        value = re.search(r"^\$\d+ = (.*)", out, re.M)
        value = value.group(1) if value else None
        told = "value has been optimized out" in said(ran.stderr, name)
        if not value and (told or f'Symbol "{name}" is optimized out.' in out):
            value = OPTIMIZED_OUT
        elif value and (told or "<error reading variable: value has been optimized out>" in value):
            value = f"{value.split('<error reading variable')[0].rstrip()} {OPTIMIZED_OUT}".lstrip()
        printed[name] = re.sub(r"0x[0-9a-f]+", "0x..", value) if value is not None else None
    return printed


def settings(tally):
    """Whether the profile's settings for what a debugger reads are followed, or refused with the reason."""
    work, ws = sline("profile", LOCALS_MAIN)
    binary, homes = None, 0

    def built(**profile):
        """The program blaze builds under the settings in an empty home: whether it has anything
        for a debugger, and how many variables."""
        nonlocal binary, homes
        names = {f"CARGO_PROFILE_DEV_{name.upper()}": value for name, value in profile.items()}
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        homes += 1
        try:
            binary = build(ws, "sline", os.path.join(work, f"home-{homes}"), env=names)
            return debug_bytes(binary) > 0, described(binary)[1], None
        except SystemExit as refused:
            return None, None, str(refused)

    line = LOCALS_MAIN[:LOCALS_MAIN.index("println!")].count("\n") + 1
    names = [name for name, _ in LOCALS]

    def shown():
        """What gdb prints of the function's values in the program as it was last built."""
        return printed_by("gdb", binary, ws, f"{MAIN}:{line}", names)

    def apart():
        """How many units of blaze's in the program have their entries in a file beside it, which is there."""
        named = re.findall(r'DW_AT_producer\s+\("blaze[ "].*?DW_AT_GNU_dwo_name\s+\("([^"]*\.dwo)"\)', "".join(units(binary)), re.S)
        beside = os.path.dirname(binary)
        return sum(os.path.exists(os.path.join(beside, name)) or os.path.exists(binary + ".dwp") for name in named)

    linked = None
    for what, profile, lines, locals_, split in (
            ("debug = true", {}, True, True, False),
            ("debug = \"line-tables-only\"", {"debug": "line-tables-only"}, True, False, False),
            ("debug = 1", {"debug": "1"}, True, False, False),
            ("strip = \"debuginfo\"", {"strip": "debuginfo"}, False, False, False),
            ("strip = true", {"strip": "true"}, False, False, False),
            ("split-debuginfo = \"off\"", {"split_debuginfo": "off"}, True, True, False),
            ("split-debuginfo = \"packed\"", {"split_debuginfo": "packed"}, True, True, True),
            ("split-debuginfo = \"unpacked\"", {"split_debuginfo": "unpacked"}, True, True, True),
            ("split-debuginfo = \"packed\" with debug = false", {"split_debuginfo": "packed", "debug": "false"}, False, False, False)):
        has, count, refused = built(**profile)
        followed = has == lines and bool(count) == locals_
        more = refused[-110:] if refused else f"{'lines' if has else 'nothing'}, {count} variables"
        if followed and locals_:
            values, kept = shown(), apart()
            linked = linked or values
            followed = values == linked and bool(kept) == split
            more += f", {kept} units kept apart, gdb prints {sum(v is not None for v in values.values())} of {len(names)} values"
            more += "" if values == linked else " which are not the ones it prints of the program that has them linked"
        tally.say(f"profile: {what}", "followed" if followed else "NOT FOLLOWED", more)
        if split and not refused:
            verify(tally, f"profile: {what}", binary)
    shutil.rmtree(work, ignore_errors=True)


def debug_bytes(binary):
    """The bytes of what a debugger reads of the program: its `.debug_` sections, or on Windows
    the PDB the linker wrote beside it."""
    if WINDOWS:
        _, name = Image(binary).pdb()
        pdb = os.path.join(os.path.dirname(binary), os.path.basename(name or "none.pdb"))
        return os.path.getsize(pdb) if os.path.exists(pdb) else 0
    read = subprocess.run(["size", "-A", binary], capture_output=True, text=True).stdout
    return sum(int(l.split()[1]) for l in read.splitlines() if l.startswith(".debug_"))


def cost(tally):
    """The stress workspace's program with and without debug info, and what the profile's setting does."""
    work = scratch("blaze-debuginfo-cost-")
    ws = copy(os.path.join(HERE, "stress"), os.path.join(work, "ws"))
    homes = 0

    def fresh(setting, home=None):
        """A build at `debug = setting` in an empty home (or `home`): (seconds, the program)."""
        nonlocal homes
        homes += 1
        if home is None:
            shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        began = time.time()
        binary = build(ws, "sapp", home or os.path.join(work, f"home-{homes}"), env={"CARGO_PROFILE_DEV_DEBUG": setting})
        return time.time() - began, binary

    measured = {}
    for setting in ("false", "true"):
        times = []
        for _ in range(3):
            took, binary = fresh(setting)
            times.append(took)
        measured[setting] = (min(times), os.path.getsize(binary), debug_bytes(binary))
    (t0, b0, d0), (t1, b1, d1) = measured["false"], measured["true"]
    tally.say("cost: the stress workspace's program without debug info", "measured", f"{t0:.2f} s to build from nothing, program {b0} bytes")
    tally.say("cost: with it", "measured" if d1 and not d0 else "NO DIFFERENCE",
              f"{t1:.2f} s ({(t1 / t0 - 1) * 100:+.1f}%), program {b1} bytes ({(b1 / b0 - 1) * 100:+.1f}%, {d1} of what a debugger reads)")
    for setting, expected in (("false", False), ("true", True), ("line-tables-only", True)):
        has = debug_bytes(fresh(setting)[1]) > 0
        tally.say(f"cost: the profile says debug = {setting}", ("described" if has else "left out") if has == expected else "NOT FOLLOWED")
    # The store is shared by the builds of every profile, and what a build of lines alone
    # kept has no variables: a build that writes them is not to take it.
    name = "cost: debug = true after line-tables-only, from one store"
    if WINDOWS:
        tally.say(name, "not run", "it counts DWARF's variables")
    else:
        alone = described(fresh("true")[1])
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        home = os.path.join(work, "shared")
        fresh("line-tables-only", home)
        forget(home)
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        after = described(fresh("true", home)[1])
        tally.say(name, "described" if after == alone else "VARIABLES LOST",
                  f"{after[0]} functions and {after[1]} variables, {alone[0]} and {alone[1]} from an empty home")
        # What a build takes from the store is what it would have compiled: Cranelift's cache
        # of code kept a frame without its last slots, and a root refused when compiled was
        # taken from the store (#242).
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        home = os.path.join(work, "again")
        fresh("true", home)
        forget(home)
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        again = described(fresh("true", home)[1])
        tally.say("cost: debug = true from the store a build at true filled", "described" if again == alone else "NOT THE SAME",
                  f"{again[0]} functions and {again[1]} variables, {alone[0]} and {alone[1]} from an empty home")
    shutil.rmtree(work, ignore_errors=True)


def main():
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    tally = Tally()
    parts = {
        "stress": lambda: program(tally, "stress", os.path.join(HERE, "stress"), ["sapp", "sbase", "smid"], "sapp",
                                  ("base/src/shapes.rs", "items.iter().map(|it| it.area()).sum()",
                                   "items.iter().map(|it| {\n        " + PANIC + "\n        it.area()\n    }).sum()")),
        "rung2": lambda: program(tally, "rung2", os.path.join(HERE, "rung2"), ["rung2"], "rung2",
                                 ("src/main.rs", ".map_ok(|it| it + 1)", ".map_ok(|it| {\n        " + PANIC + "\n        it + 1\n    })")),
        "edits": lambda: (edits(tally), edits(tally, apart=True)),
        "callers": lambda: callers(tally),
        "locals": lambda: variables(tally),
        "profile": lambda: settings(tally),
        "cost": lambda: cost(tally),
    }
    if WINDOWS:
        # These ask gdb, lldb and `llvm-dwarfdump` of what only DWARF has so far.
        for name in ("callers", "profile"):
            if only in (None, name):
                tally.say(name, "not run", "CodeView has lines and procedures alone")
            del parts[name]
        parts["edits"] = lambda: edits(tally)
    for name, part in parts.items():
        if only in (None, name):
            part()
    left = f", {tally.open} open" if tally.open else ""
    print(f"{'wrong: ' + str(tally.wrong) if tally.wrong else 'all right'}{left}")
    sys.exit(1 if tally.wrong else 0)


if __name__ == "__main__":
    main()
