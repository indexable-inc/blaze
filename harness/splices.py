#!/usr/bin/env python3
"""splices.py: edits that are hard on the splice, a step at a time under a daemon, against cargo.

The program has thread-locals, statics of every kind, vtables, tables of function pointers,
closures, generics, recursion, panics and foreign calls, and the steps edit each. Some steps
first do something to the linked binary (strip it, touch it, run it). After every step the
binary is run as it is and with everything bound at once (LD_BIND_NOW, or DYLD_BIND_AT_LAUNCH
on macOS), read by readelf (there: its signature verified and its fixups read by dyld_info), and compared with what
`cargo build` of the same source prints. How each step got linked is counted, and where SPLICED
has the target, the number of steps that were spliced has to be the one it says.

BLAZE_SPLICE=0   link each edit in full (the same test of the full link)
SPLICES_NO_PIE   link the daemon's binary with -no-pie
SPLICES_SOAK=N   after the steps, N more edits of one constant, each checked by its output
"""
import difflib, os, re, shlex, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import linkline
from difftest import Fixture, BLAZE, sysroot_rlibs, RUNNER, BINUTILS, toolchain, WINDOWS, TARGET, TARGET_ARGS, built, WASM, host

# How many of the steps are spliced, by target: the rest link in full for the reasons printed,
# or not at all where nothing was edited, and one more of them is a splice that got worse. A
# target that is not here is held to none. Once "a thread-local's initial value" is edited the
# build has a copy of its own of that thread-local (docs/live.md#statics), so the five steps
# after it carry thread-local data and link in full, as on Linux and macOS. A static grown
# past the base's copy is the patch's own (docs/live.md#types), so the steps that grow one
# splice.
SPLICED = {"x86_64-pc-windows-msvc": 59}

TOML = """[package]
name = "sedge"
version = "0.1.0"
edition = "2021"
"""
MAIN = "src/main.rs"
MAIN_TEXT = r"""use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, Ordering};

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static NAMES: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

static COUNTER: AtomicU32 = AtomicU32::new(5);
static mut RAW: u32 = 3;
static WORDS: [&str; 3] = ["one", "two", "three"];
static FIRST: &&str = &WORDS[0];
static OPS: [fn(u32) -> u32; 3] = [double, triple, tiny];
static TABLE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
static SEVEN: u32 = 7;
static EIGHT: u32 = SEVEN + 1;
static MAYBE: [Option<&u32>; 3] = [Some(&SEVEN), None, Some(&SEVEN)];
static SHORTS: [u16; 4] = [1, 2, 3, 4];

fn double(x: u32) -> u32 {
    x * 2
}

fn triple(x: u32) -> u32 {
    x * 3
}

fn tiny(_: u32) -> u32 {
    1
}

fn empty() {}

trait Speak {
    fn speak(&self) -> String;
    fn loud(&self) -> String {
        self.speak().to_uppercase()
    }
}

struct Dog;
struct Cat(u32);

impl Speak for Dog {
    fn speak(&self) -> String {
        "woof".to_string()
    }
}

impl Speak for Cat {
    fn speak(&self) -> String {
        format!("meow{}", self.0)
    }
    fn loud(&self) -> String {
        "MEOW".to_string()
    }
}

impl Drop for Cat {
    fn drop(&mut self) {
        println!("cat {} gone", self.0);
    }
}

fn generic<T: std::fmt::Debug + Clone>(x: T) -> String {
    format!("{:?}", (x.clone(), x))
}

fn fib(n: u32) -> u32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn even(n: u32) -> bool {
    n == 0 || odd(n - 1)
}

fn odd(n: u32) -> bool {
    n != 0 && even(n - 1)
}

fn deepen() -> u32 {
    DEPTH.with(|d| {
        d.set(d.get() + 1);
        d.get()
    })
}

fn remember(name: &str) -> usize {
    NAMES.with(|n| {
        n.borrow_mut().push(name.to_string());
        n.borrow().len()
    })
}

fn bump() -> u32 {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn raw() -> u32 {
    unsafe {
        RAW += 2;
        RAW
    }
}

fn pick(i: usize) -> fn(u32) -> u32 {
    OPS[i % 3]
}

fn adder(n: u32) -> impl Fn(u32) -> u32 {
    move |x| x + n
}

fn boxed(n: u32) -> Box<dyn Fn(u32) -> u32> {
    Box::new(move |x| x * n + 1)
}

fn risky(n: u32) -> u32 {
    let _cat = Cat(n);
    if n > 2 {
        panic!("risky {n}");
    }
    n + 10
}

fn caught(n: u32) -> String {
    match std::panic::catch_unwind(|| risky(n)) {
        Ok(v) => format!("ok {v}"),
        Err(_) => "caught".to_string(),
    }
}

fn threads() -> u32 {
    let handles: Vec<_> = (0..3u32)
        .map(|i| std::thread::spawn(move || deepen() + deepen() + bump() * 0 + i))
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).sum()
}

fn foreign() -> usize {
    unsafe { libc_strlen(b"hello\0".as_ptr().cast()) }
}

extern "C" {
    #[link_name = "strlen"]
    fn libc_strlen(s: *const std::ffi::c_char) -> usize;
}

fn sum_table() -> u32 {
    TABLE.iter().map(|&b| u32::from(b)).sum()
}

fn soak() -> u32 {
    1000
}

fn main() {
    std::panic::set_hook(Box::new(|info| println!("hook: {info}")));
    if let Ok(ms) = std::env::var("SEDGE_HOLD") {
        std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap()));
    }
    empty();
    println!("ops {} {} {}", pick(0)(7), pick(1)(7), pick(2)(7));
    let pets: Vec<Box<dyn Speak>> = vec![Box::new(Dog), Box::new(Cat(1))];
    for pet in &pets {
        println!("pet {} {}", pet.speak(), pet.loud());
    }
    drop(pets);
    println!("generic {} {}", generic(4u8), generic("s"));
    println!("fib {} even {} odd {}", fib(15), even(10), odd(10));
    println!("tls {} {} {}", deepen(), deepen(), remember("a") + remember("b"));
    println!("threads {}", threads());
    println!("statics {} {} {} {} {}", bump(), bump(), raw(), raw(), FIRST);
    println!("closures {} {}", adder(3)(4), boxed(5)(6));
    println!("caught {} {}", caught(1), caught(5));
    println!("foreign {}", foreign());
    println!("table {}", sum_table());
    println!("maybe {:?} shorts {:?} eight {}", MAYBE, SHORTS, EIGHT);
    println!("soak {}", soak());
}
"""


# What reads an executable and says so when it is malformed, and what has the loader bind
# everything at once.
if linkline.MACHO:
    READERS = [["codesign", "--verify", "--strict"], ["dyld_info", "-fixups", "-segments", "-exports"]]
    BIND_NOW = "DYLD_BIND_AT_LAUNCH"
else:
    READERS = [[BINUTILS + "readelf", "-lSdrW", "--dyn-syms"]]
    BIND_NOW = "LD_BIND_NOW"


def strip(binary):
    if WINDOWS:
        # The toolchain's own: a PE loses the table of symbols the linker was asked to keep.
        sysroot, host = toolchain()
        tool = os.path.join(sysroot, "lib", "rustlib", host, "bin", "rust-objcopy.exe")
        subprocess.run([tool, "--strip-all", binary], check=True)
        return
    if WASM:
        subprocess.run(["wasm-tools", "strip", binary, "-o", binary], check=True)
        return
    subprocess.run([BINUTILS + "strip", binary], check=True)


def touch(binary):
    os.utime(binary, (time.time() + 5, time.time() + 5))


def rewrite(binary):
    """The same bytes in a new file, as another build tool would leave it."""
    data = open(binary, "rb").read()
    os.remove(binary)
    open(binary, "wb").write(data)
    os.chmod(binary, 0o755)


def sound_pe(path):
    """What dumpbin has against the file, and whether the tables the loader and the unwinder
    read are in order: sections that follow one another, functions sorted and inside the
    image, every address to fix up inside a section."""
    read = subprocess.run(["dumpbin", "/nologo", "/headers", "/relocations", "/unwindinfo", "/imports", path],
                          capture_output=True, text=True)
    said = [l for l in (read.stdout + read.stderr).splitlines() if re.search(r"(LNK|warning|error)", l)]
    if read.returncode or said:
        return f"dumpbin: {(said or ['exit ' + str(read.returncode)])[0][:80]}"
    data = open(path, "rb").read()
    u16 = lambda at: int.from_bytes(data[at:at + 2], "little")
    u32 = lambda at: int.from_bytes(data[at:at + 4], "little")
    nt = u32(0x3c)
    count, optional = u16(nt + 6), nt + 24
    section_align, image_size = u32(optional + 32), u32(optional + 56)
    table = optional + u16(nt + 20)
    sections = [(u32(at + 12), u32(at + 8), u32(at + 20), u32(at + 16))
                for at in (table + 40 * n for n in range(count))]
    end = sections[0][0]
    for address, size, raw, raw_size in sections:
        if address != end:
            return f"pe: a section at {address:#x}, the one before ends at {end:#x}"
        if raw + raw_size > len(data):
            return f"pe: the section at {address:#x} is past the end of the file"
        end = (address + size + section_align - 1) // section_align * section_align
    if end != image_size:
        return f"pe: an image of {image_size:#x} whose sections end at {end:#x}"

    def at_file(address):
        for start, size, raw, raw_size in sections:
            if start <= address < start + max(size, raw_size):
                return raw + address - start
        return None

    directory = lambda n: (u32(optional + 112 + 8 * n), u32(optional + 116 + 8 * n))
    functions, size = directory(3)
    last = 0
    for at in range(at_file(functions), at_file(functions) + size, 12):
        begin, finish, unwind = u32(at), u32(at + 4), u32(at + 8)
        if begin < last or finish <= begin or at_file(begin) is None or at_file(unwind & ~1) is None:
            return f"pe: the function table is out of order at {begin:#x}"
        last = begin
    fixups, size = directory(5)
    at, stop = at_file(fixups), at_file(fixups) + size
    while at < stop:
        page, block = u32(at), u32(at + 4)
        if block < 8 or block % 4:
            return f"pe: a block of fixups of {block} bytes"
        for entry in range(at + 8, at + block, 2):
            kind, offset = u16(entry) >> 12, u16(entry) & 0xfff
            if kind not in (0, 10) or (kind and at_file(page + offset) is None):
                return f"pe: a fixup of kind {kind} at {page + offset:#x}"
        at += block
    return None


# (what the step is, [(old, new)], something done to the binary first or None)
STEPS = [
    ("nothing edited", [], None),
    # Code that is only reached through an address somebody stored.
    ("a function only a static table points to", [("x * 3\n", "x * 3 + 1\n")], None),
    ("a function shorter than a jump", [("fn tiny(_: u32) -> u32 {\n    1\n}", "fn tiny(_: u32) -> u32 {\n    2\n}")], None),
    ("an empty function given a body", [("fn empty() {}", 'fn empty() {\n    println!("not empty");\n}')], None),
    ("an empty function emptied again", [('fn empty() {\n    println!("not empty");\n}', "fn empty() {}")], None),
    ("a method in a vtable", [('"woof".to_string()', '"wuff".to_string()')], None),
    ("a default method in a vtable", [("self.speak().to_uppercase()", 'self.speak().to_uppercase() + "!"')], None),
    ("an overriding method in a vtable", [('"MEOW".to_string()', 'format!("MEOW{}", self.0)')], None),
    ("a method added to a trait and its vtables", [
        ("    fn loud(&self) -> String {\n        self.speak().to_uppercase()", "    fn soft(&self) -> String {\n        self.speak().to_lowercase()\n    }\n    fn loud(&self) -> String {\n        self.speak().to_uppercase()"),
        ('println!("pet {} {}", pet.speak(), pet.loud());', 'println!("pet {} {} {}", pet.speak(), pet.loud(), pet.soft());'),
    ], None),
    ("a type added to a vtable's trait", [
        ("struct Cat(u32);", "struct Cat(u32);\nstruct Fox;\n\nimpl Speak for Fox {\n    fn speak(&self) -> String {\n        \"ring\".to_string()\n    }\n}"),
        ("vec![Box::new(Dog), Box::new(Cat(1))]", "vec![Box::new(Dog), Box::new(Cat(1)), Box::new(Fox)]"),
    ], None),
    ("a drop in a vtable", [('println!("cat {} gone", self.0);', 'println!("cat {} is gone", self.0);')], None),
    ("a closure that is returned", [("move |x| x + n", "move |x| x + n * 2")], None),
    ("a boxed closure", [("move |x| x * n + 1", "move |x| x * n + 2")], None),
    ("the function a table is indexed in", [("OPS[i % 3]", "OPS[(i + 1) % 3]")], None),
    # Generics and recursion.
    ("a generic function's body", [('format!("{:?}", (x.clone(), x))', 'format!("<{:?}>", (x.clone(), x))')], None),
    ("a generic instantiated at a new type", [("generic(4u8), generic(\"s\")", "generic(4u8), generic(vec![1.5f32])")], None),
    ("a function that calls itself", [("fib(n - 1) + fib(n - 2)", "fib(n - 1) + fib(n - 2) + 1")], None),
    ("one of two that call each other", [("n == 0 || odd(n - 1)", "n == 0 || n == 4 || odd(n - 1)")], None),
    ("the other of the two", [("n != 0 && even(n - 1)", "n != 0 && n != 7 && even(n - 1)")], None),
    # Thread-locals.
    ("a function that writes a thread-local", [("d.set(d.get() + 1);", "d.set(d.get() + 2);")], None),
    ("a function that uses a lazy thread-local", [("n.borrow().len()", "n.borrow().len() * 10")], None),
    ("a thread-local's initial value", [("Cell::new(0)", "Cell::new(100)")], None),
    ("a thread-local added and used", [
        ("    static NAMES:", "    static EXTRA: Cell<u64> = const { Cell::new(7) };\n    static NAMES:"),
        ("fn bump() -> u32 {", "fn extra() -> u64 {\n    EXTRA.with(|e| {\n        e.set(e.get() * 3);\n        e.get()\n    })\n}\n\nfn bump() -> u32 {"),
        ('println!("threads {}", threads());', 'println!("threads {} extra {} {}", threads(), extra(), extra());'),
    ], None),
    ("a thread-local with a destructor added", [
        ("    static NAMES:", "    static GUARD: Cat = const { Cat(99) };\n    static NAMES:"),
        ("fn bump() -> u32 {", "fn guard() -> u32 {\n    GUARD.with(|g| g.0)\n}\n\nfn bump() -> u32 {"),
        ("std::thread::spawn(move || deepen()", "std::thread::spawn(move || guard() * 0 + deepen()"),
    ], None),
    ("the closure threads run", [("bump() * 0 + i", "bump() * 0 + i * 2")], None),
    ("a thread-local removed", [
        ("    static EXTRA: Cell<u64> = const { Cell::new(7) };\n", ""),
        ("fn extra() -> u64 {\n    EXTRA.with(|e| {\n        e.set(e.get() * 3);\n        e.get()\n    })\n}\n\n", ""),
        ('println!("threads {} extra {} {}", threads(), extra(), extra());', 'println!("threads {}", threads());'),
    ], None),
    # Statics.
    ("a function that adds to an atomic", [("COUNTER.fetch_add(1,", "COUNTER.fetch_add(3,")], None),
    ("an atomic's initial value", [("AtomicU32::new(5)", "AtomicU32::new(50)")], None),
    ("a function that writes a static mut", [("RAW += 2;", "RAW += 5;")], None),
    ("a static mut's initial value", [("static mut RAW: u32 = 3;", "static mut RAW: u32 = 30;")], None),
    ("a static that other statics point into", [('["one", "two", "three"]', '["uno", "dos", "tres"]')], None),
    ("a static that points into another", [("&WORDS[0]", "&WORDS[2]")], None),
    ("a table of functions reordered", [("[double, triple, tiny]", "[tiny, double, triple]")], None),
    ("a function added to a table of functions", [
        ("fn empty() {}", "fn square(x: u32) -> u32 {\n    x * x\n}\n\nfn empty() {}"),
        ("[tiny, double, triple]", "[square, double, triple]"),
    ], None),
    ("a static that stores fewer addresses", [("[Some(&SEVEN), None, Some(&SEVEN)]", "[None, None, Some(&SEVEN)]")], None),
    ("a static that stores more addresses", [("[None, None, Some(&SEVEN)]", "[Some(&SEVEN), Some(&SEVEN), Some(&SEVEN)]")], None),
    ("a static that stores none", [("[Some(&SEVEN), Some(&SEVEN), Some(&SEVEN)]", "[None, None, None]")], None),
    ("a static pointed to", [("static SEVEN: u32 = 7;", "static SEVEN: u32 = 77;")], None),
    ("a static that stores them again", [("[None, None, None]", "[None, Some(&SEVEN), None]")], None),
    ("a static another is computed from", [("static SEVEN: u32 = 77;", "static SEVEN: u32 = 700;")], None),
    ("a static of a smaller type", [("static SHORTS: [u16; 4] = [1, 2, 3, 4];", "static SHORTS: [u16; 2] = [9, 8];")], None),
    ("a static of a larger type", [("static SHORTS: [u16; 2] = [9, 8];", "static SHORTS: [u16; 3] = [9, 8, 7];")], None),
    ("a static of bytes", [("[1, 2, 3, 4, 5, 6, 7, 8]", "[8, 7, 6, 5, 4, 3, 2, 9]")], None),
    ("a static of bytes grown to a megabyte", [
        ("static TABLE: [u8; 8] = [8, 7, 6, 5, 4, 3, 2, 9];", "static TABLE: [u8; 1 << 20] = [3; 1 << 20];"),
    ], None),
    ("a static another points into, grown", [
        ('static WORDS: [&str; 3] = ["uno", "dos", "tres"];', 'static WORDS: [&str; 4] = ["uno", "dos", "drei", "cuatro"];'),
    ], None),
    ("a static added and used", [
        ("static TABLE:", "static BONUS: [u32; 4] = [10, 20, 30, 40];\nstatic TABLE:"),
        ("TABLE.iter().map(|&b| u32::from(b)).sum()", "TABLE.iter().map(|&b| u32::from(b)).sum::<u32>() + BONUS[3]"),
    ], None),
    ("a static mut added and written", [
        ("static TABLE:", "static mut TALLY: [u64; 2] = [1, 2];\nstatic TABLE:"),
        ("fn soak() -> u32 {", "fn tally() -> u64 {\n    unsafe {\n        TALLY[0] += TALLY[1];\n        TALLY[0]\n    }\n}\n\nfn soak() -> u32 {"),
        ('println!("table {}", sum_table());', 'println!("table {} tally {} {}", sum_table(), tally(), tally());'),
    ], None),
    ("a static of a function pointer added", [
        ("static TABLE:", "static LATE: fn(u32) -> u32 = double;\nstatic TABLE:"),
        ('println!("closures {} {}"', 'println!("late {}", LATE(21));\n    println!("closures {} {}"'),
    ], None),
    ("a static removed", [
        ("static BONUS: [u32; 4] = [10, 20, 30, 40];\n", ""),
        (".sum::<u32>() + BONUS[3]", ".sum::<u32>()"),
    ], None),
    ("a string constant used once", [('println!("foreign {}", foreign());', 'println!("foreign call {}", foreign());')], None),
    # Unwinding.
    ("a function that panics with a guard alive", [("if n > 2 {", "if n > 0 {")], None),
    ("its message", [('panic!("risky {n}");', 'panic!("too risky: {n}");')], None),
    ("the function that catches", [('Err(_) => "caught".to_string()', 'Err(_) => "caught it".to_string()')], None),
    ("a panic inside a drop's unwinding", [
        ("let _cat = Cat(n);", "let _cat = Cat(n);\n    let _other = Cat(n + 100);"),
    ], None),
    ("the panic hook", [('println!("hook: {info}")', 'println!("hooked: {info}")')], None),
    ("no panic any more", [("if n > 0 {", "if n > 1000 {")], None),
    # Things the program did not refer to before.
    ("a foreign function used for the first time", [
        ("    #[link_name = \"strlen\"]", "    fn abs(x: i32) -> i32;\n    #[link_name = \"strlen\"]"),
        ("unsafe { libc_strlen(b\"hello\\0\".as_ptr().cast()) }", "unsafe { libc_strlen(b\"hello\\0\".as_ptr().cast()) + abs(-4) as usize }"),
    ], None),
    ("a std function used for the first time", [
        ('println!("soak {}", soak());', 'println!("soak {}", soak());\n    println!("path {:?}", std::path::Path::new("/a/b.txt").extension());'),
    ], None),
    ("a collection used for the first time", [
        ('println!("soak {}", soak());', 'println!("soak {}", soak());\n    println!("map {:?}", [(2, "b"), (1, "a")].into_iter().collect::<std::collections::BTreeMap<_, _>>());'),
    ], None),
    ("float formatting used for the first time", [
        ('println!("soak {}", soak());', 'println!("soak {}", soak());\n    println!("float {:.3} {:e}", 2.0f64.sqrt(), 1234.5f32);'),
    ], None),
    ("an attribute that keeps a function out of line", [("fn double(x: u32)", "#[inline(never)]\n#[cold]\nfn double(x: u32)")], None),
    ("main itself", [('println!("fib {} even {} odd {}"', 'println!("FIB {} even {} odd {}"')], None),
    # The binary is not what the daemon left.
    ("an edit after the binary was touched", [("x * 2\n", "x * 2 + 1\n")], touch),
    ("the next edit", [("x * 2 + 1\n", "x * 2 + 2\n")], None),
    ("an edit after the binary was written again", [("x * 2 + 2\n", "x * 2 + 3\n")], rewrite),
    ("the next edit", [("x * 2 + 3\n", "x * 2 + 4\n")], None),
    ("an edit after the binary was stripped", [("x * 2 + 4\n", "x * 2 + 5\n")], strip),
    ("the next edit", [("x * 2 + 5\n", "x * 2 + 6\n")], None),
    ("an edit while the binary runs", [("x * 2 + 6\n", "x * 2 + 7\n")], "hold"),
    ("the next edit", [("x * 2 + 7\n", "x * 2 + 8\n")], None),
    ("an edit undone", [("x * 2 + 8\n", "x * 2 + 7\n")], None),
    ("everything of one function back as it first was", [("x * 2 + 7\n", "x * 2\n"), ("#[inline(never)]\n#[cold]\n", "")], None),
]

# `wasm32-wasip1` has no threads: what the program gives to one runs where it is asked for.
INLINE = """
mod inline {
    pub struct Ran<T>(T);
    impl<T> Ran<T> {
        pub fn join(self) -> Result<T, ()> {
            Ok(self.0)
        }
    }
    pub fn spawn<T>(f: impl FnOnce() -> T) -> Ran<T> {
        Ran(f())
    }
}
"""


def ported(text):
    return text.replace("std::thread::spawn(", "inline::spawn(") if WASM else text


def main():
    ws = tempfile.mkdtemp(prefix="blaze-splices-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", TOML), (MAIN, ported(MAIN_TEXT) + (INLINE if WASM else ""))):
        open(os.path.join(ws, path), "w").write(text)
    work = tempfile.mkdtemp(prefix="blaze-splices-out-")
    socket, log_path = os.path.join(work, "blaze.sock"), os.path.join(work, "daemon.log")
    reference = os.path.join(work, "reference")
    fx = Fixture(ws, ["sedge"], ["-p", "sedge"])
    fx.rustc_build()
    obj, patch, binary = (os.path.join(work, f"sedge.{n}") for n in ("o", "patch.o", "bin"))
    link = list(fx.link_args)
    link = linkline.with_output(linkline.ahead(link, [patch, obj], BLAZE), binary)
    if os.environ.get("SPLICES_NO_PIE"):
        link = [a for a in link if a != "-pie"] + ["-no-pie"]
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    splice = ["--binary", binary]
    ask = ["--crate", "sedge", "--out", obj, "--patch", patch, "--link-cmd", linkline.join(link), *splice, *symbols]
    out = open(log_path, "a")
    daemon = subprocess.Popen([BLAZE, ws, *TARGET_ARGS, "--socket", socket, *ask], stderr=out, stdout=out)
    while "watching" not in open(log_path).read():
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon did not start: {log_path}")
        time.sleep(0.05)

    def run(path, env=None):
        try:
            ran = subprocess.run([*RUNNER, path], capture_output=True, text=True, timeout=30, env={**os.environ, **(env or {})})
        except Exception as error:
            return f"did not run: {error}"
        return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")

    def sync():
        seen = len(open(log_path).read())
        reply = subprocess.run([BLAZE, "sync", socket, *ask], capture_output=True, text=True, timeout=120)
        links = re.findall(r"^link: \S+ \((.*)\)$", open(log_path).read()[seen:], re.M)
        how = "no link" if not links else "spliced" if links[-1].startswith("spliced") else \
            "full: " + (re.search(r"not spliced: (.*)", links[-1]) or [None, "not asked to splice"])[1]
        return (reply.stdout + reply.stderr).strip(), how

    def sound(path):
        """What the tools that read an executable have against the file."""
        if WINDOWS:
            return sound_pe(path)
        if WASM:
            read = subprocess.run(["wasm-tools", "validate", path], capture_output=True, text=True)
            return f"invalid: {read.stderr.strip()[:80]}" if read.returncode else None
        for tool in READERS:
            read = subprocess.run([*tool, path], capture_output=True, text=True)
            said = [l for l in read.stderr.splitlines() if l.strip()]
            if read.returncode or said:
                return f"{os.path.basename(tool[0])}: {(said or ['exit ' + str(read.returncode)])[0][:80]}"
        return None

    def edit(edits):
        text = open(os.path.join(ws, MAIN)).read()
        for old, new in edits:
            old, new = ported(old), ported(new)
            assert old in text, old
            text = text.replace(old, new, 1)
        open(os.path.join(ws, MAIN), "w").write(text)

    wrong, hows = 0, {}
    for what, edits, before in STEPS:
        held = None
        if before == "hold" and RUNNER:
            # The kernel refuses a write to a file it runs, which is what has this edit linked in
            # full. An emulator only maps the file: nothing can be held against the write.
            what += " (not run meanwhile: " + os.path.basename(RUNNER[0]) + ")"
        elif before == "hold":
            expected = run(binary)
            held = subprocess.Popen([*RUNNER, binary], stdout=subprocess.PIPE, text=True, env={**os.environ, "SEDGE_HOLD": "1500"})
            time.sleep(0.3)
        elif before:
            before(binary)
        edit(edits)
        time.sleep(0.02)
        began = time.time()
        reply, how = sync()
        took = (time.time() - began) * 1000
        cargo = subprocess.run(["cargo", "build", "-q", *TARGET_ARGS], cwd=ws, capture_output=True, text=True,
                               env={**os.environ, "CARGO_TARGET_DIR": reference})
        assert cargo.returncode == 0, cargo.stderr[-2000:]
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon died at {what!r}: {log_path}")
        theirs = run(built(reference, "sedge"))
        ours = None
        if not reply.startswith("ok"):
            verdict = "REFUSED: " + reply[:60]
        elif (ours := run(binary)) != theirs:
            verdict = "DIFFERENT OUTPUT"
        elif (ours := run(binary, {BIND_NOW: "1"})) != theirs:
            verdict = "DIFFERENT OUTPUT BOUND AT ONCE"
        elif held and held.communicate(timeout=30)[0] != expected:
            verdict = "THE RUNNING ONE CHANGED"
        else:
            verdict = sound(binary) or "same"
            verdict = verdict.upper() if verdict != "same" else verdict
        if held:
            held.wait(timeout=30)
        wrong += verdict != "same"
        kind = how.split(":")[0] if verdict == "same" else "wrong"
        hows[how] = hows.get(how, 0) + 1
        print(f"{what:48} {verdict:16} {took:5.0f} ms  {how}", flush=True)
        if verdict.startswith("DIFFERENT OUTPUT"):
            # Whether the binary prints the same again, and cargo's too, tells a wrong build
            # from a program whose output changes from run to run.
            again = run(binary, {BIND_NOW: "1"} if verdict.endswith("AT ONCE") else None) == ours
            print(f"    blaze's prints {'the same' if again else 'otherwise'} when run again, cargo's "
                  f"{'the same' if run(built(reference, 'sedge')) == theirs else 'otherwise'}")
            for line in difflib.unified_diff(theirs.splitlines(), ours.splitlines(), "cargo's", "blaze's", n=0, lineterm=""):
                print(f"    {line}")

    soak = int(os.environ.get("SPLICES_SOAK", "0"))
    soak_wrong = 0
    if soak:
        value, spliced, times = 1000, 0, []
        for turn in range(soak):
            edit([(f"    {value}\n}}\n\nfn main()", f"    {value + 1}\n}}\n\nfn main()")])
            value += 1
            began = time.time()
            reply, how = sync()
            times.append((time.time() - began) * 1000)
            spliced += how == "spliced"
            printed = run(binary)
            if not reply.startswith("ok") or f"soak {value}\n" not in printed or (turn % 50 == 49 and sound(binary)):
                soak_wrong += 1
                print(f"soak {turn}: WRONG ({how}): {reply[:60]} {printed[-40:]!r}", flush=True)
        cargo = subprocess.run(["cargo", "build", "-q", *TARGET_ARGS], cwd=ws, capture_output=True, text=True,
                               env={**os.environ, "CARGO_TARGET_DIR": reference})
        same = run(binary) == run(built(reference, "sedge"))
        soak_wrong += not same
        times.sort()
        print(f"soak: {soak - soak_wrong} of {soak} right, {spliced} spliced, median {times[len(times) // 2]:.0f} ms per request, "
              f"binary {os.path.getsize(binary)} bytes, the last one {'same as' if same else 'DIFFERENT FROM'} cargo's")
    daemon.kill()
    for how, count in sorted(hows.items(), key=lambda it: -it[1]):
        print(f"  {count:3} {how}")
    print(f"{len(STEPS) - wrong} of {len(STEPS)} right; log {log_path}; source {ws}")
    spliced, expected = hows.get("spliced", 0), SPLICED.get(TARGET or host())
    if expected not in (None, spliced):
        print(f"{spliced} steps spliced, where {expected} do")
        wrong += 1
    sys.exit(1 if wrong or soak_wrong else 0)


if __name__ == "__main__":
    main()
