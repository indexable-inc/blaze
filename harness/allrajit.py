#!/usr/bin/env python3
"""allrajit.py: programs with nothing rustc built in them (docs/no-rustc.md, steps F and G).

Five programs, a hello, one that uses collections, `catch_unwind`, threads, channels,
formatting, a file and an uncaught panic, one with a `#[global_allocator]` of its own, one
built with `panic = "abort"`, and one with a static of proc-macro clients as J2 writes it,
are built by ra-jit with every crate its own, the standard library included, and linked by
ra-jit (`--exe`): the native libraries are the ones the crates' `#[link]`s ask for, and
nothing it could not compile may be left in the executable. A shared library is linked too
(`--exe` with `--shared`), loaded with `dlopen` and called as cargo's `cdylib` of it is, and
a proc-macro crate's, whose list of macros ra-jit writes into it is read off the library.
Each is built three ways:

  - whole: the program and the standard library in one build, one object;
  - over the unit: the standard library built once, alone, into an object of its own
    (`std.o`), and each program built over it with `--prebuilt`;
  - stored: each program built alone on a store that starts empty and a remote (a directory)
    that does too. The first build compiles the standard library as a unit, keeps it in the
    store and pushes it; the others take it from the store. Last, a build on a second store
    that starts empty takes it from the remote.

What each prints, and how it exits, has to be what cargo's build of the same source does,
thread ids aside.

    python3 allrajit.py            # every way
    python3 allrajit.py unit stored
"""
import json, os, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import RA_JIT
from storelib import scratch

# The crates of the standard library a program on Linux links, by the names cargo gives them,
# but the panic runtime.
STD = ["std", "core", "alloc", "compiler_builtins", "unwind", "libc", "cfg_if", "hashbrown",
       "rustc_demangle", "std_detect", "addr2line", "gimli", "object", "miniz_oxide", "adler2",
       "memchr"]


def std_of(panic):
    return STD + ["panic_abort" if panic == "abort" else "panic_unwind"]


HELLO = """fn main() {
    let v: Vec<u32> = (1..=10).collect();
    println!("hello {} {:?}", v.iter().sum::<u32>(), std::env::args().count());
}
"""
RICH = r"""use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Arc, Mutex, mpsc};

#[derive(Debug, Clone, PartialEq)]
enum Shape { Circle(f64), Rect { w: f64, h: f64 } }

trait Area { fn area(&self) -> f64; }
impl Area for Shape {
    fn area(&self) -> f64 {
        match self { Shape::Circle(r) => std::f64::consts::PI * r * r, Shape::Rect { w, h } => w * h }
    }
}

fn main() {
    let mut words = HashMap::new();
    for w in "the quick brown fox jumps over the lazy dog the end".split(' ') {
        *words.entry(w).or_insert(0usize) += 1;
    }
    let sorted: BTreeMap<_, _> = words.iter().collect();
    println!("words {:?}", sorted);
    let shapes: Vec<Box<dyn Area>> = vec![Box::new(Shape::Circle(1.5)), Box::new(Shape::Rect { w: 2.0, h: 3.25 })];
    let total: f64 = shapes.iter().map(|s| s.area()).sum();
    println!("area {total:.6} {:e} {}", total, 1.0f64 / 3.0);
    let caught = std::panic::catch_unwind(|| {
        let v: Vec<i32> = Vec::new();
        v[3]
    });
    println!("caught {}", caught.is_err());
    let r = std::panic::catch_unwind(|| -> i32 { panic!("boom {}", 7) });
    let payload = r.unwrap_err();
    let msg = payload.downcast_ref::<String>().cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
    println!("msg {msg}");
    let counter = Arc::new(Mutex::new(0u64));
    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (0..4u64).map(|i| {
        let counter = Arc::clone(&counter);
        let tx = tx.clone();
        std::thread::spawn(move || {
            for j in 0..1000 { *counter.lock().unwrap() += i * j; }
            tx.send(i).unwrap();
        })
    }).collect();
    drop(tx);
    for h in handles { h.join().unwrap(); }
    let mut got: Vec<u64> = rx.iter().collect();
    got.sort();
    println!("threads {} {:?}", *counter.lock().unwrap(), got);
    let mut s = String::new();
    write!(s, "{:>8}|{:<6}|{:^7}|{:#x}|{:08.3}", "right", "left", "mid", 255, -3.14159).unwrap();
    println!("{s}");
    let text = std::fs::read_to_string("Cargo.toml").unwrap();
    println!("lines {}", text.lines().count());
    println!("parsed {:?} {:?}", "42".parse::<i64>(), "x".parse::<u8>().is_err());
    std::io::stdout().flush().unwrap();
    if std::env::args().count() > 1 { let none: Option<u8> = None; none.expect("asked to fail"); }
}
"""
ALLOC = r"""use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) { unsafe { System.dealloc(ptr, layout) } }
}

#[global_allocator]
static A: Counting = Counting;

fn main() {
    let before = ALLOCS.load(Ordering::Relaxed);
    let v: Vec<Box<u64>> = (0..100).map(Box::new).collect();
    let s: String = v.iter().map(|b| b.to_string()).collect();
    println!("{} {} counted {}", s.len(), v.len(), ALLOCS.load(Ordering::Relaxed) - before >= 101);
}
"""
ABORT = r"""fn main() {
    let caught = std::panic::catch_unwind(|| 1);
    println!("caught {:?}", caught.ok());
    if std::env::args().count() > 1 { panic!("aborting {}", 3); }
}
"""
# What docs/no-rustc.md's J2 writes into a proc-macro crate: a static of each macro's client,
# which reaches `proc_macro`'s own statics (its symbol interner's thread-local) through the
# bridge it runs. The standard library's unit leaves `proc_macro` out, so the program has it.
MACROS = r"""#![feature(proc_macro_internals)]
#![allow(internal_features)]
extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro::bridge::client::Client;

fn derive_it(input: TokenStream) -> TokenStream { input }

fn attr_it(_attr: TokenStream, item: TokenStream) -> TokenStream { item }

#[unsafe(no_mangle)]
pub static ALLRAJIT_MACROS: [Client; 2] = [Client::expand1(derive_it), Client::expand2(attr_it)];

fn main() {
    let words = std::hint::black_box(&ALLRAJIT_MACROS) as *const _ as *const usize;
    let (first, second) = unsafe { (*words, *words.add(1)) };
    println!("clients {} {}", first != 0 && second != 0, first != second);
}
"""
# (program, its source, the runs to compare: the arguments of each, its panic strategy)
PROGRAMS = [("hello", HELLO, [["a", "b"]], "unwind"), ("rich", RICH, [[], ["fail"]], "unwind"),
            ("counting", ALLOC, [[]], "unwind"), ("abort", ABORT, [[], ["fail"]], "abort"),
            ("macros", MACROS, [[]], "unwind")]
# A shared library, as J2 links a proc macro's (`--exe` with `--shared`): its function
# allocates, formats, keeps a thread-local and catches a panic, which the allocator shim, the
# library's TLS and the unwinder have to be right for, loaded into a process ra-jit did not link.
LIBRARY = r"""use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static SEEN: RefCell<BTreeMap<u64, String>> = RefCell::new(BTreeMap::new());
}

#[unsafe(no_mangle)]
pub extern "C" fn allrajit_count(n: u64) -> u64 {
    let text: String = (0..n).map(|i| format!("{i},")).collect();
    SEEN.with(|seen| seen.borrow_mut().insert(n, text.clone()));
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| if n == 3 { panic!("three") } else { n });
    let seen = SEEN.with(|seen| seen.borrow().len() as u64);
    seen * 1000 + text.len() as u64 * 10 + caught.is_err() as u64
}
"""
CALLS = [1, 3, 10, 3]
# A proc-macro crate linked as J2 links one: the library has the list of its macros ra-jit
# writes into the crate (crates/ra-jit/src/macro_list.rs) where rustc's metadata has it.
PROC_MACROS = r"""extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Hello, attributes(hello, greet))]
pub fn hello(_input: TokenStream) -> TokenStream {
    "impl Hello for () {}".parse().unwrap()
}

#[proc_macro_attribute]
pub fn same(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}
"""
# Kind (derive, attribute, function-like), name and helper attributes, by name.
MACRO_LIST = [[0, "Hello", ["hello", "greet"]], [2, "answer", []], [1, "same", []]]
# cargo's build of these needs the unstable features they name.
UNSTABLE = {"macros"}
# Crates of the standard library a program links that the library's unit leaves out. What
# they depend on (`rustc-literal-escaper` of `proc_macro`) makes the unit another one.
LINKS = {"macros": ["proc_macro"]}


def run(cmd, cwd, what, env=None):
    start = time.monotonic()
    ran = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0", **(env or {})})
    took = time.monotonic() - start
    if ran.returncode != 0:
        raise SystemExit(f"FAIL: {what} exited {ran.returncode}:\n{ran.stdout}{ran.stderr}")
    return ran, took


def outcome(binary, args, cwd):
    ran = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0"})
    unthreaded = lambda text: re.sub(r"thread '([^']*)' \(\d+\)", r"thread '\1' (N)", text)
    return ran.returncode, unthreaded(ran.stdout), unthreaded(ran.stderr)


def package(work, name, source, panic):
    root = os.path.join(work, name)
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n[workspace]\n')
        if panic == "abort":
            f.write('[profile.dev]\npanic = "abort"\n')
    with open(os.path.join(root, "src/main.rs"), "w") as f:
        f.write(source)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def library_package(work):
    root = os.path.join(work, "shared")
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "shared"\nversion = "0.1.0"\nedition = "2021"\n'
                '[lib]\ncrate-type = ["cdylib"]\n[workspace]\n')
    with open(os.path.join(root, "src/lib.rs"), "w") as f:
        f.write(LIBRARY)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def proc_macro_package(work):
    root = os.path.join(work, "macros-crate")
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "allmacros"\nversion = "0.1.0"\nedition = "2021"\n'
                '[lib]\nproc-macro = true\n[workspace]\n')
    with open(os.path.join(root, "src/lib.rs"), "w") as f:
        f.write(PROC_MACROS)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def macro_list(library):
    """The list of macros the library exports, each with whether its client is there, read by a
    process of its own."""
    code = ("import ctypes, json, sys\n"
            "class Slice(ctypes.Structure):\n"
            "    _fields_ = [('ptr', ctypes.c_void_p), ('len', ctypes.c_size_t)]\n"
            "class Macro(ctypes.Structure):\n"
            "    _fields_ = [('kind', ctypes.c_uint32), ('name', Slice), ('helpers', Slice),\n"
            "                ('client', ctypes.c_void_p)]\n"
            "text = lambda it: ctypes.string_at(it.ptr, it.len).decode()\n"
            "listed = Slice.in_dll(ctypes.CDLL(sys.argv[1]), 'RA_JIT_PROC_MACROS')\n"
            "macros = (Macro * listed.len).from_address(listed.ptr)\n"
            "helpers = lambda it: [text(h) for h in (Slice * it.len).from_address(it.ptr or 1)]\n"
            "print(json.dumps([[m.kind, text(m.name), helpers(m.helpers), bool(m.client)]\n"
            "                  for m in macros]))\n")
    ran = subprocess.run([sys.executable, "-c", code, library], capture_output=True, text=True)
    return ran.returncode, ran.stdout, ran.stderr


def calls(library):
    """What the library's `allrajit_count` gives for each of CALLS, loaded by a process of its own."""
    code = ("import ctypes, sys\n"
            "f = ctypes.CDLL(sys.argv[1]).allrajit_count\n"
            "f.argtypes, f.restype = [ctypes.c_uint64], ctypes.c_uint64\n"
            f"print([f(n) for n in {CALLS!r}])\n")
    ran = subprocess.run([sys.executable, "-c", code, library], capture_output=True, text=True)
    return ran.returncode, ran.stdout, ran.stderr


def ra_jit(root, crates, out, *extra, env=None):
    args = sum((["--crate", c] for c in crates), [])
    return run([RA_JIT, root, *args, "--out", out, *extra], root, f"ra-jit {' '.join(crates[:1])}", env)


def linked(ran, what):
    """How long ra-jit's link took, failing when it left something it could not compile."""
    said = re.search(r"^linked .* in (\S+), (\d+) functions in it ra-jit could not compile$",
                     ran.stderr, re.M)
    if not said:
        raise SystemExit(f"FAIL: {what}: ra-jit did not say it linked:\n{ran.stderr}")
    if said.group(2) != "0":
        raise SystemExit(f"FAIL: {what}: the executable can reach {said.group(2)} functions ra-jit "
                         f"could not compile:\n{ran.stderr}")
    return said.group(1)


def main():
    if not sys.platform.startswith("linux"):
        print("not run: the programs ra-jit links with nothing of rustc's are Linux's so far")
        return
    ways = [way for way in ["whole", "unit", "stored"] if way in sys.argv[1:] or len(sys.argv) == 1]
    work = scratch("ra-jit-allrajit-")
    passed = False
    try:
        roots = {name: package(work, name, source, panic) for name, source, _, panic in PROGRAMS}
        expected = {}
        for name, _, runs, _ in PROGRAMS:
            root = roots[name]
            target = os.path.join(root, "target")
            run(["cargo", "build", "-q", "--target-dir", target], root, f"cargo build of {name}",
                {"RUSTC_BOOTSTRAP": "1"} if name in UNSTABLE else None)
            binary = os.path.join(root, "target/debug", name)
            expected[name] = [outcome(binary, args, root) for args in runs]
        library, macros = library_package(work), proc_macro_package(work)
        run(["cargo", "build", "-q", "--target-dir", os.path.join(library, "target")], library,
            "cargo build of the shared library")
        expected_calls = calls(os.path.join(library, "target/debug/libshared.so"))
        if expected_calls[0] != 0:
            raise SystemExit(f"FAIL: cargo's shared library did not load: {expected_calls}")
        lines = []
        units = {}
        if "unit" in ways:
            for panic, program in [("unwind", "hello"), ("abort", "abort")]:
                units[panic] = os.path.join(work, f"std-{panic}.o")
                _, took = ra_jit(roots[program], std_of(panic), units[panic])
                lines.append(f"the standard library alone, panic = \"{panic}\": {took:.2f} s, "
                             f"{os.path.getsize(units[panic]) >> 20} MB")
        remote = {"RA_JIT_REMOTE": os.path.join(work, "remote"), "RA_JIT_PROJECT": "allrajit",
                  "RA_JIT_REMOTE_UNSIGNED": "1"}
        os.makedirs(remote["RA_JIT_REMOTE"])
        stores = [{"RA_JIT_CACHE": os.path.join(work, "store"), **remote},
                  {"RA_JIT_CACHE": os.path.join(work, "store2"), **remote}]
        for way in ways:
            # The first program again once the store has the library, and the second on
            # another store.
            programs = PROGRAMS + PROGRAMS[:1] + PROGRAMS[1:2] if way == "stored" else PROGRAMS
            # The units, by panic strategy and what else of the library is linked, that some
            # store or the remote has.
            compiled = set()
            for at, (name, _, runs, panic) in enumerate(programs):
                root = roots[name]
                obj = os.path.join(work, f"{name}-{way}.o")
                binary = os.path.join(work, f"{name}-{way}")
                exe = ["--exe", binary]
                how = ""
                own = [name, *LINKS.get(name, [])]
                if way == "whole":
                    ran, took = ra_jit(root, [*own, *std_of(panic)], obj, *exe)
                elif way == "unit":
                    ran, took = ra_jit(root, own, obj, "--prebuilt", units[panic], *exe)
                else:
                    store = stores[at == len(programs) - 1]
                    ran, took = ra_jit(root, own, obj, *exe, env=store)
                    said = re.findall(r"^standard library: (.*)$", ran.stderr, re.M)
                    stored = bool(said) and said[0].startswith("from the store")
                    unit = (panic, *LINKS.get(name, []))
                    if not said or stored != (unit in compiled):
                        raise SystemExit(f"FAIL: {name} ({way}, build {at + 1}) "
                                         f"{'should have taken' if unit in compiled else 'took'} the "
                                         f"standard library from the store: {said}\n{ran.stderr}")
                    compiled.add(unit)
                    how = (", the standard library from the remote" if store is stores[1]
                           else ", the standard library from the store" if stored
                           else ", the standard library compiled, stored and pushed")
                link_took = linked(ran, f"{name} ({way})")
                for args, want in zip(runs, expected[name]):
                    got = outcome(binary, args, root)
                    if got != want:
                        raise SystemExit(f"FAIL: {name} {' '.join(args)} ({way}) differs from cargo's:\n"
                                         f"cargo:  {want}\nra-jit: {got}")
                lines.append(f"{name} ({way}): built and linked in {took:.2f} s{how}, the link "
                             f"{link_took}, {len(runs)} runs as cargo's")
            obj, shared = (os.path.join(work, f"shared-{way}{ext}") for ext in (".o", ".so"))
            exe = ["--exe", shared, "--shared"]
            if way == "whole":
                ran, took = ra_jit(library, ["shared", *std_of("unwind")], obj, *exe)
            elif way == "unit":
                ran, took = ra_jit(library, ["shared"], obj, "--prebuilt", units["unwind"], *exe)
            else:
                ran, took = ra_jit(library, ["shared"], obj, *exe, env=stores[0])
                said = re.findall(r"^standard library: (.*)$", ran.stderr, re.M)
                if not said or not said[0].startswith("from the store"):
                    raise SystemExit(f"FAIL: the shared library (stored) should have taken the "
                                     f"standard library from the store: {said}\n{ran.stderr}")
            link_took = linked(ran, f"the shared library ({way})")
            got = calls(shared)
            if got != expected_calls:
                raise SystemExit(f"FAIL: the shared library ({way}) differs from cargo's:\n"
                                 f"cargo:  {expected_calls}\nra-jit: {got}")
            lines.append(f"the shared library ({way}): built and linked in {took:.2f} s, the link "
                         f"{link_took}, {len(CALLS)} calls as cargo's library answers them")
            obj, shared = (os.path.join(work, f"allmacros-{way}{ext}") for ext in (".o", ".so"))
            exe = ["--exe", shared, "--shared"]
            if way == "whole":
                ran, took = ra_jit(macros, ["allmacros", "proc_macro", *std_of("unwind")], obj, *exe)
            elif way == "unit":
                ran, took = ra_jit(macros, ["allmacros", "proc_macro"], obj,
                                   "--prebuilt", units["unwind"], *exe)
            else:
                ran, took = ra_jit(macros, ["allmacros", "proc_macro"], obj, *exe, env=stores[0])
            link_took = linked(ran, f"the proc-macro crate ({way})")
            code, out, err = macro_list(shared)
            want = [[*it, True] for it in MACRO_LIST]
            if code != 0 or json.loads(out) != want:
                raise SystemExit(f"FAIL: the proc-macro crate's library ({way}) lists other than "
                                 f"its macros:\nwanted: {want}\ngot:    {code} {out}{err}")
            lines.append(f"the proc-macro crate ({way}): built and linked in {took:.2f} s, the "
                         f"link {link_took}, its {len(MACRO_LIST)} macros listed")
        print("\n".join(lines))
        print(f"PASS: {len(PROGRAMS)} programs, a shared library and a proc-macro crate's with "
              f"nothing of rustc's, "
              f"{' and '.join(ways)}")
        passed = True
    finally:
        if passed:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
