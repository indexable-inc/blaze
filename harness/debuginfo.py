#!/usr/bin/env python3
"""debuginfo.py: what a debugger and a backtrace are told of blaze's code, against rustc's.

Four programs are built by cargo and again with their crates from blaze: the corpus runner, the
stress workspace and rung2 with a panic put into a copy of each, and a small program of its own
that a daemon then follows through edits, each added to the linked binary where that can be done.
Every one is run with RUST_BACKTRACE=1 and has to print, for each panic, the frames of the
workspace's own files that the cargo build prints: the same functions at the same file and line.
`llvm-dwarfdump --verify` has to find nothing wrong with any of the binaries, and gdb has to stop
at a line of a function blaze compiled, say where it is, and step to other lines of it.

On Windows what a debugger reads is the PDB `link.exe /DEBUG` writes from the CodeView of the
objects, so `llvm-pdbutil` reads it in place of `llvm-dwarfdump`, and `cdb` is the debugger. Only
lines and procedures are written there: no variable is told of, and the names of functions are
held against the cargo build's by the last part of each, which the two spell differently
(`pick::<u8>` for `pick<u8>`). A splice leaves the PDB as it was, and that is what is checked
of it: the old lines stay true, and the code that was added has no line rather than a wrong one.

Then the cost: the time and the size of the corpus's object with and without debug info,
whether blaze leaves it out when the profile says `debug = false`, and whether it works out
the types of variables only when the profile asks for variables.

BLAZE_BACKEND=llvm   the same of LLVM's code. Its optimizer turns calls in tail position into
                     jumps, so a frame the cargo build prints may be missing: those are counted.
--only NAME          one of: corpus stress rung2 edits callers locals types scopes rich profile cost
"""
import bisect, glob, os, re, shutil, struct, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.setdefault("CARGO_PROFILE_DEV_DEBUG", "true")
import linkline
from difftest import Fixture, BLAZE, WINDOWS, sysroot_rlibs, built

LLVM = os.environ.get("BLAZE_BACKEND") == "llvm"
# The pinned LLVM's, on PATH in the flake's dev shell and in CI.
DWARFDUMP = shutil.which("llvm-dwarfdump") or "llvm-dwarfdump"
PDBUTIL = shutil.which("llvm-pdbutil") or os.path.join(os.environ.get("LLVM_SYS_231_PREFIX", r"C:\llvm"), "bin", "llvm-pdbutil.exe")
CDB = shutil.which("cdb") or r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe"
DEBUGGER = "cdb" if WINDOWS else "gdb"
PANIC = 'if std::env::var_os("DEBUGINFO_PANIC").is_some() { drop(std::panic::take_hook()); panic!("deliberate"); }'
# What rustc calls the code that drops a coroutine, which blaze has the coroutine itself do.
DROP_GLUE = re.compile(r"^core::ptr::drop_glue::<(.*)>$")
# The same in CodeView, where rustc calls a coroutine `async_fn$0` and what it keeps `async_fn_env$0`.
COROUTINE = re.compile(r"\b(?:async_gen|async|gen)_(?:fn|block)\$(\d+)")
DROP_GLUE_CODEVIEW = re.compile(r"^core::ptr::drop_glue<enum2\$<(.*)::(?:async_gen|async|gen)_(?:fn|block)_env\$(\d+)> >$")

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
OPTIMIZED_OUT = "<optimized out>"

TYPES_MAIN = """#[derive(Clone, Copy)]
struct Point { x: i32, y: i64 }
struct Pair(u8, f64);
#[derive(Clone, Copy)]
enum Shape { Circle(u32), Rect { w: u16, h: u16 }, Dot }
#[derive(Clone, Copy)]
enum Level { Low = 1, Mid = 5, High = 9 }
union Bits { whole: u32, half: u16 }

fn show(width: u32, name: &str, big: [u64; 5]) -> u32 {
    let point = Point { x: 3, y: -4 };
    let pair = Pair(7, 1.5);
    let tuple = (true, 'c', 9u64);
    let shape = Shape::Rect { w: 4, h: 5 };
    let circle = Shape::Circle(11);
    let dot = Shape::Dot;
    let level = Level::Mid;
    let some = Some(42u32);
    let none: Option<u32> = None;
    let boxed: Option<&Point> = Some(&point);
    let missing: Option<&Point> = None;
    let ok: Result<u8, String> = Ok(3);
    let list = vec![1i32, 2, 3];
    let text = String::from("hello");
    let slice = &list[1..];
    let bits = Bits { whole: 0x10002 };
    let borrowed = &point;
    let sum = width + point.x as u32 + pair.0 as u32 + tuple.2 as u32 + big[4] as u32;
    println!("{} {} {} {}", sum, name, borrowed.y, slice.len());
    std::hint::black_box((&shape, &circle, &dot, &level, &some, &none, &boxed, &missing, &ok, &text, &bits, &tuple));
    sum
}

fn main() {
    println!("{}", show(10, "abc", [1, 2, 3, 4, 5]));
}
"""

# Names bound more than once and names of an inner block. Each `// stop` is a line a debugger
# stops at, where the names of SCOPED are asked for: the answer depends on the block.
SCOPES_MAIN = """use std::hint::black_box;

fn scoped(seed: u32) -> u64 {
    let value = seed + 1;
    black_box(value); // stop
    let value = u64::from(value) * 3;
    let total = {
        let inner = value + 5;
        let value = "text";
        black_box(inner) + black_box(value.len() as u64) // stop
    };
    black_box(total); // stop
    let mut sum = 0;
    for step in 0..2u64 {
        let value = [step, total];
        sum += black_box(value[1]); // stop
    }
    match black_box(Some(sum)) {
        Some(value) if value > 1 => black_box(value + total), // stop
        _ => 0,
    }
}

fn main() {
    println!("{}", scoped(black_box(4)));
}
"""
# Closures, what an `async fn` keeps, pointers to `dyn` and enums with fields or without a tag
# of their own: the types a debugger's printers for Rust look into the shape of.
RICH_MAIN = """use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

trait Named { fn name(&self) -> u32; }
struct Dog { legs: u32 }
impl Named for Dog { fn name(&self) -> u32 { self.legs } }
enum Tree { Leaf, Node(Box<Tree>, u32) }
enum Wide { A(u64, u8), B { text: &'static str }, C }

struct Once(bool);
impl Future for Once {
    type Output = u32;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u32> {
        if self.0 { Poll::Ready(7) } else { self.0 = true; Poll::Pending }
    }
}

async fn steps(start: u32) -> u32 {
    let first = Once(false).await + start;
    let kept = [first, 2];
    let second = Once(false).await;
    kept[0] + second
}

fn show(seed: u32) -> u32 {
    let offset = seed + 1;
    let mut count = 0u64;
    let by_ref = |x: u32| x + offset;
    let by_move = move |x: u32| x * offset;
    let mut by_mut = || { count += 1; count };
    let dog = Dog { legs: 4 };
    let object: &dyn Named = &dog;
    let boxed: Box<dyn Named> = Box::new(Dog { legs: 3 });
    let leaf = Tree::Leaf;
    let node = Tree::Node(Box::new(Tree::Leaf), 5);
    let wide = Wide::A(9, 1);
    let wide_b = Wide::B { text: "hi" };
    let wide_c = Wide::C;
    let nested: Option<Option<bool>> = Some(None);
    let ordering = std::cmp::Ordering::Less;
    let mut future = Box::pin(steps(seed));
    let fresh = steps(seed);
    let mut cx = Context::from_waker(Waker::noop());
    let _ = future.as_mut().poll(&mut cx);
    let once = &*future;
    let _ = std::hint::black_box(&once);
    let sum = by_ref(1) + by_move(2) + by_mut() as u32 + object.name() + boxed.name();
    println!("{}", sum);
    std::hint::black_box((&leaf, &node, &wide, &wide_b, &wide_c, &nested, &ordering, &fresh));
    let _ = future.as_mut().poll(&mut cx);
    let _ = future.as_mut().poll(&mut cx);
    sum
}

fn main() {
    println!("{}", show(std::hint::black_box(10)));
}
"""
RICH = ["by_ref", "by_move", "by_mut", "object", "boxed", "leaf", "node", "wide", "wide_b", "wide_c", "nested",
        "ordering", "fresh", "*once", "future"]
SCOPED = ["seed", "value", "inner", "total", "sum", "step"]
TYPES = ["width", "name", "big", "point", "pair", "tuple", "shape", "circle", "dot", "level", "some", "none", "boxed",
         "missing", "ok", "list", "text", "slice", "bits", "*borrowed", "sum"]

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


def panics(stderr):
    """The panics a run printed: where each says it was and what it said, and its frames as
    [name, place]."""
    found, lines = [], stderr.splitlines()
    for at, line in enumerate(lines):
        if "panicked at" in line:
            said = lines[at + 1].strip() if at + 1 < len(lines) else ""
            found.append((line.split("panicked at")[1].strip() + " " + said, []))
        elif found and (frame := re.match(r"\s+\d+: (.*)", line)):
            found[-1][1].append([frame.group(1).strip(), None])
        elif found and found[-1][1] and (place := re.match(r"\s+at (.*)", line)):
            found[-1][1][-1][1] = place.group(1).strip()
    return found


def last_part(name):
    """What a function is called without what it is in and what it is given: `pick` of both
    `sline::pick::<u8>`, which is how a symbol of rustc's reads, and `sline::pick<u8>`, which
    is how rustc names it in CodeView. A closure keeps what it is a closure of."""
    if glue := DROP_GLUE_CODEVIEW.match(name):
        name = f"{glue.group(1)}::closure${glue.group(2)}"
    name = COROUTINE.sub(r"closure$\1", name)
    parts, depth, start, at = [], 0, 0, 0
    while at < len(name):
        if name[at] in "<({[":
            depth += 1
        elif name[at] in ")}]" or (name[at] == ">" and name[at - 1] != "-"):
            depth -= 1
        elif depth == 0 and name.startswith("::", at):
            parts.append(name[start:at])
            start, at = at + 2, at + 1
        at += 1
    parts = [re.sub(r"^\{closure#(\d+)\}$", r"closure$\1", part) for part in parts + [name[start:]]]
    named = [part.split("<")[0] for part in parts if part and not part.startswith("<")] or [name]
    keep = len(named) - 1
    while keep > 0 and named[keep].startswith("closure$"):
        keep -= 1
    return "::".join(named[keep:])


def own(frames):
    """The frames of the workspace's own files, by function and file:line."""
    kept = []
    for name, place in frames:
        place = place.replace("\\", "/") if place else ""
        if not place.startswith("./"):
            continue
        if WINDOWS:
            # A frame has no column there.
            kept.append((last_part(name), place))
            continue
        glue = DROP_GLUE.match(name)
        kept.append((glue.group(1) if glue else name, place.rsplit(":", 1)[0]))
    return kept


def within(ours, theirs):
    """Whether `ours` is `theirs` with frames left out."""
    rest = iter(theirs)
    return all(frame in rest for frame in ours)


def backtraces(binary, cwd, env=None):
    ran = subprocess.run([binary], capture_output=True, text=True, errors="replace", cwd=cwd, timeout=900,
                         env={**os.environ, "RUST_BACKTRACE": "1", **(env or {})})
    return panics(ran.stderr)


BEHIND = "behind rustc"


# What is known to be wrong, and why.
OPEN = set()


class Tally:
    def __init__(self):
        self.wrong, self.open, self.lines = 0, 0, []

    def say(self, what, verdict, more=""):
        if what in OPEN and verdict.isupper():
            more, verdict = f"{verdict}: {more}", "open"
        elif what in OPEN:
            more, verdict = f"{verdict}: take it out of OPEN in harness/debuginfo.py", "SAME NOW"
        self.wrong += verdict.isupper()
        # Open: nothing shown is wrong, but rustc -O -g shows a value this has optimized out.
        self.open += verdict in (BEHIND, "open")
        print(f"{what:58} {verdict:14} {more}", flush=True)


def jumps_away(binary, name):
    """Whether the function blaze compiled under that name leaves by jumping to another one
    rather than calling it, so that it has no frame while the other runs. Its range is what the
    debug info says, and the jump is read from the disassembly; None where that cannot be read."""
    said = dumped(binary, f"--name={name}")
    entries = [it for it in said.split("\n\n") if "DW_TAG_subprogram" in it]
    if not entries and re.fullmatch(r"\w+(::\w+)*", name):
        # A function with a symbol of its own is named by its last part and its mangled path.
        parts = name.split("::")
        said = dumped(binary, f"--name={parts[-1]}")
        mangled = "".join(f"{len(part)}{part}" for part in parts)
        entries = [it for it in said.split("\n\n") if "DW_TAG_subprogram" in it
                   and re.search(rf'DW_AT_linkage_name\s+\("_R\w*?{mangled}"\)', it)]
    ranges = [(low.group(1), high.group(1)) for it in entries
              if (low := re.search(r"DW_AT_low_pc\s+\((0x[0-9a-f]+)\)", it))
              and (high := re.search(r"DW_AT_high_pc\s+\((0x[0-9a-f]+)\)", it))]
    # A function whose entry is kept apart from the program has its code as a list of one range.
    ranges += [ends for it in entries for ends in re.findall(r"\[(0x[0-9a-f]+), (0x[0-9a-f]+)\)", it)]
    if not ranges or not shutil.which("objdump"):
        return None
    for low, high in ((int(low, 16), int(high, 16)) for low, high in ranges):
        listed = subprocess.run(["objdump", "-d", "--no-show-raw-insn", f"--start-address={low:#x}",
                                 f"--stop-address={high:#x}", binary], capture_output=True, text=True).stdout
        targets = [int(it, 16) for it in re.findall(r"^\s*[0-9a-f]+:\s+(?:jmp|b)\s+([0-9a-f]+)\b", listed, re.M)]
        if any(not low <= target < high for target in targets):
            return True
        # A call through a pointer in tail position is a jump through a register. A jump table's
        # is one too, but has the function's code after it: the tail call is the last instruction.
        last = re.findall(r"^\s*[0-9a-f]+:\s+(\S.*)$", listed, re.M)[-1:]
        if last and re.match(r"(?:notrack\s+)?jmp\s+\*|br\s+x\d+\b", last[0]):
            return True
    return False


def compare(tally, what, theirs, ours, binary=None):
    """Holds the frames of each panic of blaze's binary against the cargo build's."""
    differ, fewer, frames, shown, absent = 0, 0, 0, "", set()
    for (their_at, their_frames), (our_at, our_frames) in zip(theirs, ours):
        want, got = own(their_frames), own(our_frames)
        frames += len(got)
        if their_at == our_at and want == got:
            continue
        if LLVM and their_at == our_at and got and within(got, want):
            fewer += 1
            absent.update(frame[0] for frame in want if frame not in got)
            continue
        differ += 1
        shown = shown or f"at {our_at}: {[g for g in got if g not in want][:2]} for {[w for w in want if w not in got][:2]}"
    if not theirs or len(theirs) != len(ours):
        return tally.say(what, "NOT THE PANICS", f"{len(ours)} for {len(theirs)}")
    if not frames:
        return tally.say(what, "NO FRAMES")
    more = f"{len(theirs)} panics, {frames} frames"
    if fewer:
        # A frame may only be missing because its function left by a jump: anything else is a
        # frame the debug info lost.
        left = {name: jumps_away(binary, name) if binary and not WINDOWS else None for name in sorted(absent)}
        kept = [name for name, jumps in left.items() if jumps is False]
        if kept:
            return tally.say(what, "FRAMES LOST", f"{more}; {kept[0]} calls and has no frame")
        seen = sum(1 for jumps in left.values() if jumps)
        more += (f", {fewer} with frames the optimizer left out: {seen} of {len(left)} functions seen to leave"
                 f" by a jump, first {next(iter(left))[:60]}")
    tally.say(what, "DIFFERENT" if differ else "same", more + (f"; {differ} differ, first {shown}" if differ else ""))


def dumped(binary, *what):
    """What `llvm-dwarfdump` says of the program, read from the files beside it too where a
    unit's entries are kept apart (split DWARF), with their addresses as the program has them."""
    return subprocess.run([DWARFDUMP, "--dwo", *what, binary], capture_output=True, text=True).stdout


def units(binary):
    """What each unit's own entry says. A unit kept apart is two entries, the one left in the
    program with the addresses and the one beside it with the producer: they are put together."""
    heads = [it.split("\n\n")[0] for it in dumped(binary, "--debug-info", "--show-children=0").split("DW_TAG_compile_unit")[1:]]
    ident = lambda head: (re.search(r"DW_AT_GNU_dwo_id\s+\((\w+)\)", head) or [None, None])[1]
    left = {ident(head): head for head in heads if ident(head) and "DW_AT_producer" not in head}
    return [head + left.get(ident(head), "") for head in heads if "DW_AT_producer" in head or ident(head) not in left]


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


def stale(tally, what, theirs, ours, said):
    """Holds the frames of a binary whose PDB a splice left behind against the cargo build's.

    The PDB knows nothing of the code a splice adds, so a frame in it has no line, and the
    name it gets is whatever symbol is nearest below. What has to hold is that no frame says
    a line the cargo build does not, and that the daemon said `said` of the splice."""
    if not theirs or len(theirs) != len(ours):
        return tally.say(what, "NOT THE PANICS", f"{len(ours)} for {len(theirs)}")
    wrong, lost, frames = 0, 0, 0
    for (their_at, their_frames), (our_at, our_frames) in zip(theirs, ours):
        want, got = own(their_frames), own(our_frames)
        frames += len(want)
        lost += len(want) - len(got)
        wrong += their_at != our_at or not within(got, want)
    if wrong:
        return tally.say(what, "WRONG LINE", f"{wrong} of {len(theirs)} panics have a frame the cargo build does not")
    if lost and not said:
        return tally.say(what, "NOT SAID", f"{lost} of {frames} frames have no line and the daemon said nothing of the PDB")
    tally.say(what, "stale" if lost else "same", f"{len(theirs)} panics, {lost} of {frames} frames without a line, none with a wrong one"
              + (": the daemon says the PDB is of the code before the splice" if lost else ""))


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


def own_ranges(binary):
    """Where the code of each function of blaze's is, as its units say."""
    ours = set()
    for unit in units(binary):
        if re.search(r'DW_AT_producer\s+\("blaze[ "]', unit):
            ours |= {(int(a, 16), int(b, 16)) for a, b in re.findall(r"\[0x([0-9a-f]+), 0x([0-9a-f]+)\)", unit)}
    return ours


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


def unit_of(binary, address):
    producer = re.search(r'DW_AT_producer\s+\("([^"]*)"\)', dumped(binary, f"--lookup={address:#x}"))
    return producer.group(1) if producer else "nothing"


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


def program(tally, what, workspace, crates, package, inject=None):
    ws = workspace
    if inject:
        ws = tempfile.mkdtemp(prefix=f"blaze-debuginfo-{what}-")
        shutil.copytree(workspace, ws, dirs_exist_ok=True, ignore=shutil.ignore_patterns("target"))
        path, old, new = inject
        text = open(os.path.join(ws, path)).read()
        assert old in text, old
        open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
    env = {"DEBUGINFO_PANIC": "1"} if inject else {}
    fx = Fixture(ws, crates, ["-p", package])
    fx.rustc_build()
    fx.blaze_build()
    theirs, ours = backtraces(fx.reference, fx.ws, env), backtraces(fx.binary, fx.ws, env)
    compare(tally, f"{what}: frames of each panic", theirs, ours, fx.binary)
    verify(tally, what, fx.binary)
    unwinding(tally, what, fx.binary)
    if place := call_site(ours):
        debugger(tally, what, fx.binary, fx.ws, place[2:], env)
    else:
        tally.say(f"{what}: {DEBUGGER}", "NO PLACE")
    functions, count = described(fx.binary)
    tally.say(f"{what}: what a debugger is told of", "counted", f"{functions} functions, {count} parameters and locals in them")
    if not inject:
        profiler(tally, what, fx.binary, fx.ws, crates + [package])
    return fx


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


def edited(tally, part):
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-edits-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, EDITS_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    work = tempfile.mkdtemp(prefix="blaze-debuginfo-out-")
    socket, log_path = os.path.join(work, "blaze.sock"), os.path.join(work, "daemon.log")
    reference = os.path.join(work, "reference")
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    obj, patch, binary = (os.path.join(work, f"sline.{n}") for n in ("o", "patch.o", "bin"))
    link = linkline.with_output(linkline.ahead(list(fx.link_args), [patch, obj], BLAZE), binary)
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    ask = ["--crate", "sline", "--out", obj, "--patch", patch, "--link-cmd", linkline.join(link), "--binary", binary, *symbols]
    out = open(log_path, "a")
    daemon = subprocess.Popen([BLAZE, ws, "--socket", socket, *ask], stderr=out, stdout=out)
    while "watching" not in open(log_path).read():
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon did not start: {open(log_path).read()[-2000:]}")
        time.sleep(0.05)
    spliced, linked, first = 0, True, []
    for what, changes in EDITS:
        seen = len(open(log_path).read())
        text = open(os.path.join(ws, MAIN)).read()
        for old, new in changes:
            assert old in text, old
            text = text.replace(old, new, 1)
        open(os.path.join(ws, MAIN), "w").write(text)
        time.sleep(0.02)
        reply = subprocess.run([BLAZE, "sync", socket, *ask], capture_output=True, text=True, timeout=120)
        cargo = subprocess.run(["cargo", "build", "-q"], cwd=ws, capture_output=True, text=True,
                               env={**os.environ, "CARGO_TARGET_DIR": reference})
        assert cargo.returncode == 0, cargo.stderr[-2000:]
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon died at {what!r}: {log_path}")
        links = re.findall(r"^link: \S+ \((.*)\)$", open(log_path).read()[seen:], re.M)
        how = "not linked" if not links else "spliced" if links[-1].startswith("spliced") else "linked in full"
        spliced += how == "spliced"
        # The debug info a first splice after a link adds to is copied while the daemon waits
        # (#12): what it copies itself is the section names, if any.
        if how == "spliced" and linked and (copied := re.search(r", (\d+) copied$", links[-1])):
            first.append(int(copied[1]))
        linked = how == "linked in full" or (linked and how == "not linked")
        if not (reply.stdout + reply.stderr).strip().startswith("ok"):
            tally.say(f"{part}: {what}", "REFUSED", (reply.stdout + reply.stderr).strip()[:60])
            continue
        theirs, ours = backtraces(built(reference, "sline"), ws), backtraces(binary, ws)
        if WINDOWS and how == "spliced":
            said = "debug: the PDB is of the code before the splice" in open(log_path).read()[seen:]
            stale(tally, f"{part}: {what} ({how})", theirs, ours, said)
        else:
            compare(tally, f"{part}: {what} ({how})", theirs, ours)
        verify(tally, f"{part}: {what}", binary)
        unwinding(tally, f"{part}: {what}", binary)
    ours = backtraces(binary, ws)
    if WINDOWS and how == "spliced":
        # The lines the PDB has are of code a jump now leads away from.
        tally.say(f"{part}: the last binary: cdb", "not asked", "its PDB is of the code before the splices: BLAZE_SPLICE=0 asks")
    elif place := call_site(ours):
        debugger(tally, f"{part}: the last binary", binary, ws, place[2:])
    if not WINDOWS:
        line = open(os.path.join(ws, MAIN)).read().split("must(length)")[0].count("\n") + 1
        theirs, mine = (printed_by("gdb", it, ws, f"{MAIN}:{line}", ["at"])["at"] for it in (built(reference, "sline"), binary))
        right = mine == theirs or (LLVM and mine == "<optimized out>")
        tally.say(f"{part}: the last binary: gdb prints an argument", "as cargo's" if right else "NOT AS CARGO'S", f"at = {mine}")
    if os.environ.get("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO"):
        kept = sum("DW_AT_GNU_dwo_name" in unit and bool(re.search(r'DW_AT_producer\s+\("blaze[ "]', unit)) for unit in units(binary))
        tally.say(f"{part}: units of the last binary kept apart", "some" if kept else "NONE", str(kept))
    daemon.kill()
    if os.environ.get("BLAZE_SPLICE") != "0":
        tally.say(f"{part}: added to the linked binary", "spliced" if spliced else "NONE SPLICED", f"{spliced} of {len(EDITS)} steps")
    if sys.platform.startswith("linux") and first:
        copied = max(first)
        tally.say(f"{part}: first splices after a link", "copied nothing" if copied < 1 << 16 else "COPIED DEBUG INFO",
                  f"{copied} bytes at most, of {len(first)}")


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
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-locals-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, LOCALS_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    fx.blaze_build()
    verify(tally, "locals", fx.binary)
    if WINDOWS:
        functions, count = described(fx.binary)
        return tally.say("locals: cdb prints them", "GUESSED" if count else "not told of",
                         f"{count} described in {functions} functions: no types are written to `.debug$T` yet")
    line = LOCALS_MAIN[:LOCALS_MAIN.index("println!")].count("\n") + 1
    commands = ["set pagination off", "set startup-with-shell off", f"break {MAIN}:{line}", "run",
                *[x for name, _ in LOCALS for x in (f"echo <{name}>\\n", f"print {name}")], "kill"]
    ran = subprocess.run(["gdb", "-q", "-nx", "-batch", *[x for c in commands for x in ("-ex", c)], fx.binary],
                         capture_output=True, text=True, errors="replace", cwd=ws, timeout=300)
    printed = {name: re.search(rf"<{re.escape(name)}>\n\$\d+ = (.*)", ran.stdout) for name, _ in LOCALS}
    right = [name for name, value in LOCALS if printed[name] and printed[name].group(1) == value]
    functions, count = described(fx.binary)
    gone = [name for name, _ in LOCALS if printed[name] and OPTIMIZED_OUT in printed[name].group(1)]
    wrong = [f"{name} = {printed[name].group(1) if printed[name] else 'unknown'}" for name, _ in LOCALS
             if name not in right and name not in gone]
    fine, said = lost(gone, kept_by_rustc(ws, {f"{MAIN}:{line}": gone} if gone else {}))
    tally.say("locals: gdb prints them", "NOT AS THEY ARE" if wrong else "printed" if fine else BEHIND,
              f"{len(right)} of {len(LOCALS)}, {len(gone)} optimized out, {count} described in {functions} functions"
              + (f", but {wrong[0][:50]}" if wrong else "") + said)


def callers(tally):
    """Whether panics behind inlined and `#[track_caller]` functions say where they were, and
    have the frames a cargo build's have."""
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-callers-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, CALLERS_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    fx.blaze_build()
    compare(tally, "callers: frames of each panic", backtraces(fx.reference, ws), backtraces(fx.binary, ws), fx.binary)
    verify(tally, "callers", fx.binary)


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


def optimized_out(ours, theirs):
    """Whether gdb has no whole value for a name, and shows what part it has as it shows that of
    rustc's build: the start of a value printed in part has to be the start of rustc's."""
    if not ours or OPTIMIZED_OUT not in ours:
        return False
    part = ours.removesuffix(f" {OPTIMIZED_OUT}")
    return part == ours or theirs is not None and theirs.startswith(part)


def kept_by_rustc(ws, asked):
    """Which of the values the optimizer of blaze's build did away with rustc's keeps: the
    names gdb prints a value for at the same place of `rustc -O -g` on the same source, at
    the level blaze optimizes at. `asked` is the names by their place. What rustc keeps,
    blaze has to: LLVM has the value. The result is those names, each with its place."""
    if not asked:
        return []
    env = dict(os.environ, CARGO_PROFILE_RELEASE_DEBUG="true", CARGO_TARGET_DIR=os.path.join(ws, "target-optimized"),
               CARGO_PROFILE_RELEASE_OPT_LEVEL=os.environ.get("BLAZE_OPT", "2"))
    out = subprocess.run(["cargo", "build", "--release", "-p", "sline"], cwd=ws, capture_output=True, text=True, env=env)
    if out.returncode != 0:
        raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
    binary = os.path.join(ws, "target-optimized", "release", "sline")
    kept = []
    for place, names in asked.items():
        theirs = printed_by("gdb", binary, ws, place, names)
        kept += [f"{name} at {place}" for name in names
                 if theirs[name] and not any(it in theirs[name] for it in (OPTIMIZED_OUT, "<synthetic pointer>", "not available"))]
    return kept


def lost(gone, kept):
    """What is said of the values optimized out, and whether none of them is one rustc keeps."""
    if not gone:
        return True, ""
    if kept:
        return False, f"; rustc -O -g keeps {len(kept)} of them: {', '.join(kept)[:160]}"
    return True, f"; rustc -O -g has none of them either: {', '.join(gone)[:200]}"


# What lldb says of a block in several runs of a unit kept apart (split DWARF 4): it reads the
# list of the runs without the unit's base in `.debug_ranges`, in a rustc build of more than one
# unit as well, and shows no name of the block.
LLDB_NO_RANGES = "which has a base that is less than the function's low PC"


def lldb_path():
    """The search path `rust-lldb` finds an `lldb` on, or None: distributions name theirs by version."""
    if shutil.which("lldb"):
        return os.environ["PATH"]
    for found in sorted(glob.glob("/usr/lib/llvm-*/bin/lldb"), reverse=True):
        return os.path.dirname(found) + os.pathsep + os.environ["PATH"]
    return None


def shown_by_lldb(binary, cwd, file, line, names):
    """What `rust-lldb` shows for each of the names when stopped at a line, addresses aside."""
    commands = [f"breakpoint set --file {file} --line {line}", "run", *[f"frame variable {name}" for name in names], "kill"]
    ran = subprocess.run(["rust-lldb", "--batch", *[x for c in commands for x in ("-o", c)], binary],
                         capture_output=True, text=True, errors="replace", cwd=cwd, timeout=300,
                         env={**os.environ, "PATH": lldb_path()})
    shown = {}
    for name in names:
        found = re.search(rf"^\(lldb\) frame variable {re.escape(name)}\n(.*?)(?=^\(lldb\) )", ran.stdout, re.S | re.M)
        # An address of nothing stays what it is: lldb shows bytes it has no place for as zeros.
        shown[name] = re.sub(r"0x0*[1-9a-f][0-9a-f]*", "0x..", found.group(1).strip()) if found and found.group(1).strip() else None
    return shown, ran.stderr.count(LLDB_NO_RANGES)


def agreement(names, theirs, ours, lost=()):
    """How many of the names a debugger shows as it does for a rustc build, which it has no
    value for or only part of one, and which it shows otherwise. The names are returned with
    what is said. `lost` are names known to have no whole value, for a debugger that does not
    say so itself."""
    partly = ("<synthetic pointer>", "not available")
    gone = [name for name in names if ours[name] != theirs[name]
            and (name in lost or optimized_out(ours[name], theirs[name]) or ours[name] and any(it in ours[name] for it in partly))]
    wrong = [f"{name} = {ours[name]}, not {theirs[name]}" for name in names if ours[name] != theirs[name] and name not in gone]
    unknown = [name for name in names if theirs[name] is None]
    return (not wrong and not unknown, gone,
            f"{len(names) - len(wrong) - len(gone)} of {len(names)}, {len(gone)} optimized out"
            + (f", but {wrong[0][:160]}" if wrong else "") + (f", rustc's {unknown[0]} unknown" if unknown else ""))


def rich(tally):
    """Whether closures, the states of an `async fn`, pointers to `dyn` and enums are shown as
    those of a rustc build are, by the toolchain's gdb and by its lldb."""
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-rich-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, RICH_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    fx.blaze_build()
    verify(tally, "rich", fx.binary)
    reference = os.path.join(ws, "target", "debug", "sline")
    line = RICH_MAIN[:RICH_MAIN.index('println!("{}", sum)')].count("\n") + 1
    gone = []
    if shutil.which("rust-gdb"):
        theirs = printed_by("rust-gdb", reference, ws, f"{MAIN}:{line}", RICH)
        # A name of blaze's own for the table a `dyn` pointer has is no part of what is shown.
        ours = {name: it and re.sub(r" <\.L[^>]*>", "", it)
                for name, it in printed_by("rust-gdb", fx.binary, ws, f"{MAIN}:{line}", RICH).items()}
        same, gone, said = agreement(RICH, theirs, ours)
        fine, more = lost(gone, kept_by_rustc(ws, {f"{MAIN}:{line}": gone} if gone else {}))
        tally.say("rich: rust-gdb shows them", "NOT AS RUSTC'S" if not same else "as rustc's" if fine else BEHIND, said + more)
    else:
        tally.say("rich: rust-gdb shows them", "not looked at", "no rust-gdb to ask")
    if shutil.which("rust-lldb") and lldb_path():
        theirs, _ = shown_by_lldb(reference, ws, MAIN, line, RICH)
        ours, unread = shown_by_lldb(fx.binary, ws, MAIN, line, RICH)
        # lldb shows zeros where a value is there in part, so what gdb found to be so is not held against it.
        same, _, said = agreement(RICH, theirs, ours, gone)
        if unread and not same:
            tally.say("rich: rust-lldb shows them", "not looked at",
                      f"this lldb reads {unread} runs of blocks kept apart from the wrong place; {said}")
        else:
            tally.say("rich: rust-lldb shows them", "as rustc's" if same else "NOT AS RUSTC'S", said)
    else:
        tally.say("rich: rust-lldb shows them", "not looked at", "no rust-lldb or no lldb to ask")


def shapes(tally):
    """Whether the debugger shows values of each kind of type as it shows those of a rustc build.

    The one the toolchain wraps with its pretty-printers is asked, so a `Vec` or a `String` is
    shown by what it holds only when the description has the shape the printers look for."""
    debugger = shutil.which("rust-gdb")
    if not debugger:
        return tally.say("types: the debugger shows them", "not looked at", "no rust-gdb to ask")
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-types-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, TYPES_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    fx.blaze_build()
    verify(tally, "types", fx.binary)
    place = f"{MAIN}:{TYPES_MAIN[:TYPES_MAIN.index('println!')].count(chr(10)) + 1}"
    theirs = printed_by(debugger, os.path.join(ws, "target", "debug", "sline"), ws, place, TYPES)
    ours = printed_by(debugger, fx.binary, ws, place, TYPES)
    unknown = [name for name in TYPES if theirs[name] is None]
    out = [name for name in TYPES if optimized_out(ours[name], theirs[name])]
    wrong = [f"{name} = {ours[name]}, not {theirs[name]}" for name in TYPES if ours[name] != theirs[name] and name not in out]
    fine, said = lost(out, kept_by_rustc(ws, {place: out} if out else {}))
    tally.say("types: the debugger shows them", "NOT AS RUSTC'S" if wrong or unknown else "as rustc's" if fine else BEHIND,
              f"{len(TYPES) - len(wrong) - len(out)} of {len(TYPES)}, {len(out)} optimized out"
              + (f", but {wrong[0][:90]}" if wrong else "") + (f", rustc's {unknown[0]} unknown" if unknown else "") + said)


def scopes(tally):
    """Whether a name is known where its block says and means the binding of that block: at
    several lines the debugger is asked for the same names, and has to answer as it does for a
    rustc build, also where the answer is that there is no such name."""
    debugger = shutil.which("rust-gdb")
    if not debugger:
        return tally.say("scopes: names by their block", "not looked at", "no rust-gdb to ask")
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-scopes-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, SCOPES_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()
    fx.blaze_build()
    verify(tally, "scopes", fx.binary)
    stops = [at + 1 for at, line in enumerate(SCOPES_MAIN.split("\n")) if "// stop" in line]
    asked = right = unknown = 0
    wrong, gone = [], {}
    for line in stops:
        theirs = printed_by(debugger, os.path.join(ws, "target", "debug", "sline"), ws, f"{MAIN}:{line}", SCOPED)
        ours = printed_by(debugger, fx.binary, ws, f"{MAIN}:{line}", SCOPED)
        for name in SCOPED:
            asked += 1
            if ours[name] == theirs[name]:
                right += 1
                unknown += theirs[name] is None
            elif optimized_out(ours[name], theirs[name]):
                gone.setdefault(f"{MAIN}:{line}", []).append(name)
            else:
                wrong.append(f"{name} at line {line} = {ours[name]}, not {theirs[name]}")
    names = [f"{name} at line {place.rsplit(':', 1)[1]}" for place, at in gone.items() for name in at]
    fine, said = lost(names, kept_by_rustc(ws, gone))
    tally.say("scopes: names by their block", "NOT AS RUSTC'S" if wrong else "as rustc's" if fine else BEHIND,
              f"{right} of {asked} at {len(stops)} lines ({unknown} of them not a name there), {len(names)} optimized out"
              + (f", but {'; '.join(wrong[:3])[:200]}" if wrong else "") + said)


def settings(tally):
    """Whether the profile's settings for what a debugger reads are followed, or refused with the reason."""
    ws = tempfile.mkdtemp(prefix="blaze-debuginfo-profile-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", EDITS_TOML), (MAIN, LOCALS_MAIN)):
        open(os.path.join(ws, path), "w").write(text)
    fx = Fixture(ws, ["sline"], ["-p", "sline"])
    fx.rustc_build()

    def built(**profile):
        """The object blaze writes under the settings: whether it has anything for a debugger, and how many variables."""
        names = {f"CARGO_PROFILE_DEV_{name.upper()}": value for name, value in profile.items()}
        os.environ.update(names)
        try:
            fx.blaze_build()
            return sections(fx.objects[0])[1] > 0, described(fx.objects[0])[1], None
        except SystemExit as refused:
            return None, None, str(refused)
        finally:
            for name in names:
                del os.environ[name]

    line = LOCALS_MAIN[:LOCALS_MAIN.index("println!")].count("\n") + 1
    names = [name for name, _ in LOCALS]

    def shown():
        """What gdb prints of the function's values in the program as it was last built."""
        return printed_by("gdb", fx.binary, ws, f"{MAIN}:{line}", names)

    def apart():
        """How many units of blaze's in the program have their entries in a file beside its object, which is there."""
        named = re.findall(rf'DW_AT_producer\s+\("blaze[ "].*?DW_AT_GNU_dwo_name\s+\("({re.escape(fx.objects[0])}[^"]*\.dwo)"\)', "".join(units(fx.binary)), re.S)
        return sum(os.path.exists(name) for name in named)

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
            verify(tally, f"profile: {what}", fx.binary)


def sections(path):
    """The size of an object, and of the sections of it a debugger reads."""
    if WINDOWS:
        data = open(path, "rb").read()
        count, = struct.unpack_from("<H", data, 2)
        headers = [struct.unpack_from("<8s8xI", data, 20 + 40 * i) for i in range(count)]
        return len(data), sum(size for name, size in headers if name.startswith(b".debug$"))
    read = subprocess.run(["size", "-A", path], capture_output=True, text=True).stdout
    debug = sum(int(l.split()[1]) for l in read.splitlines() if l.startswith((".debug_", ".rela.debug_")))
    return os.path.getsize(path), debug


def cost(tally):
    """The corpus's object with and without debug info, and what the profile's setting does."""
    fx = Fixture(os.path.join(HERE, "corpus"), ["cases"], ["-p", "runner"])
    fx.rustc_build()
    measured = {}
    for setting in ("0", "1"):
        os.environ["BLAZE_DEBUGINFO"] = setting
        times = []
        for _ in range(3):
            began = time.time()
            fx.blaze_build()
            times.append(time.time() - began)
        measured[setting] = (min(times), *sections(fx.objects[0]), os.path.getsize(fx.binary))
    del os.environ["BLAZE_DEBUGINFO"]
    (t0, s0, d0, b0), (t1, s1, d1, b1) = measured["0"], measured["1"]
    told = f"{d1} of sections a debugger reads, {s1 - s0 - d1} of their relocations"
    tally.say("cost: the corpus's object without debug info", "measured", f"{t0:.2f} s to compile and link, object {s0} bytes, binary {b0} bytes")
    tally.say("cost: with it", "measured" if d1 and not d0 else "NO DIFFERENCE",
              f"{t1:.2f} s ({(t1 / t0 - 1) * 100:+.1f}%), object {s1} bytes ({(s1 / s0 - 1) * 100:+.1f}%, {told}), binary {b1} bytes")
    for setting, expected, typed in (("false", False, False), ("true", True, True), ("line-tables-only", True, False)):
        os.environ["CARGO_PROFILE_DEV_DEBUG"] = setting
        os.environ["BLAZE_MEMORY"] = memory = tempfile.mkdtemp(prefix="debuginfo-memory-")
        try:
            fx.blaze_build()
        finally:
            del os.environ["BLAZE_MEMORY"]
        has = sections(fx.objects[0])[1] > 0
        tally.say(f"cost: the profile says debug = {setting}", ("described" if has else "left out") if has == expected else "NOT FOLLOWED")
        # The types of variables are worked out only for a build that writes the variables:
        # at `line-tables-only` they are thrown away, which on a large workspace was most of
        # the cold build's memory.
        told = told_types(memory)
        shutil.rmtree(memory, ignore_errors=True)
        tally.say(f"cost: types of variables at debug = {setting}",
                  ("worked out" if told else "not worked out") if (told is not None and bool(told) == typed) else "NOT FOLLOWED",
                  "no memory report" if told is None else f"{told} told of")
    # The store is shared by the builds of every profile, and what a build of lines alone
    # kept has no variables: a build that writes them is not to take it.
    name = "cost: debug = true after line-tables-only, from one store"
    if WINDOWS:
        tally.say(name, "not run", "it counts DWARF's variables")
    else:
        def variables(settings, store):
            """Builds at each of `settings` from one store, and the variables of the object and
            what blaze's units of the binary tell of after each."""
            before = os.environ.pop("BLAZE_CACHE", None)
            if store:
                os.environ["BLAZE_CACHE"] = tempfile.mkdtemp(prefix="debuginfo-store-")
            told = []
            try:
                for setting in settings:
                    os.environ["CARGO_PROFILE_DEV_DEBUG"] = setting
                    fx.blaze_build()
                    dump = subprocess.run([DWARFDUMP, "--debug-info", fx.objects[0]], capture_output=True, text=True).stdout
                    told.append((dump.count("DW_TAG_variable"), described(fx.binary)))
            finally:
                if store:
                    shutil.rmtree(os.environ.pop("BLAZE_CACHE"), ignore_errors=True)
                if before is not None:
                    os.environ["BLAZE_CACHE"] = before
            return told
        # Against a build at `true` from a store a build at `true` filled.
        (_, first), (same, again) = variables(["true", "true"], True)
        after = variables(["line-tables-only", "true"], True)[-1][0]
        tally.say(name, "described" if after == same else "VARIABLES LOST", f"{after} variables, {same} after one at true")
        # What a build takes from the store is what it would have compiled: Cranelift's cache
        # of code kept a frame without its last slots, and a root refused when compiled was
        # taken from the store (#242).
        tally.say("cost: debug = true from the store a build at true filled", "described" if again == first else "NOT THE SAME",
                  f"{again[0]} functions and {again[1]} variables, {first[0]} and {first[1]} from an empty store")
    os.environ["CARGO_PROFILE_DEV_DEBUG"] = "true"


def told_types(memory):
    """How many types of variables the build told of, from the report `BLAZE_MEMORY` has it write."""
    reports = glob.glob(os.path.join(memory, "*.tsv"))
    if not reports:
        return None
    counts = [int(row[1]) for path in reports for row in (l.rstrip("\n").split("\t") for l in open(path))
              if len(row) > 5 and row[0] == "query" and row[5] == "told_type"]
    return max(counts, default=0)


def main():
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    tally = Tally()
    parts = {
        "corpus": lambda: program(tally, "corpus", os.path.join(HERE, "corpus"), ["cases"], "runner"),
        "stress": lambda: program(tally, "stress", os.path.join(HERE, "stress"), ["sapp", "sbase", "smid"], "sapp",
                                  ("base/src/shapes.rs", "items.iter().map(|it| it.area()).sum()",
                                   "items.iter().map(|it| {\n        " + PANIC + "\n        it.area()\n    }).sum()")),
        "rung2": lambda: program(tally, "rung2", os.path.join(HERE, "rung2"), ["rung2"], "rung2",
                                 ("src/main.rs", ".map_ok(|it| it + 1)", ".map_ok(|it| {\n        " + PANIC + "\n        it + 1\n    })")),
        "edits": lambda: (edits(tally), edits(tally, apart=True)),
        "callers": lambda: callers(tally),
        "locals": lambda: variables(tally),
        "types": lambda: shapes(tally),
        "scopes": lambda: scopes(tally),
        "rich": lambda: rich(tally),
        "profile": lambda: settings(tally),
        "cost": lambda: cost(tally),
    }
    if WINDOWS:
        # These ask gdb, lldb and `llvm-dwarfdump` of what only DWARF has so far.
        for name in ("callers", "types", "scopes", "rich", "profile"):
            if only in (None, name):
                tally.say(name, "not run", "CodeView has lines and procedures alone")
            del parts[name]
        parts["edits"] = lambda: edits(tally)
    for name, part in parts.items():
        if only in (None, name):
            part()
    left = f", {tally.open} open" if tally.open else ""
    print(f"{'wrong: ' + str(tally.wrong) if tally.wrong else 'all right'}{left} ({'llvm' if LLVM else 'cranelift'})")
    sys.exit(1 if tally.wrong else 0)


if __name__ == "__main__":
    main()
