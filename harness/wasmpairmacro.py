#!/usr/bin/env python3
"""A proc macro whose functions return pairs, run as WebAssembly, against the MIR evaluator.

    wasmpairmacro.py ['{"keep": true}']

The input, when given, is JSON: `keep` (bool, default false) keeps the work directory.

On wasm32 rustc gives a function that returns a pair of scalars `PassMode::Pair`, which LLVM
returns through a pointer it passes first (wasm32's `canLowerReturn`), and one wider than two
pointers its return place. blaze decided the pair's shape by its own `wasm` cargo feature, so a
blaze built as wasm made other IR of the same function (lane wasmabi, 2026-11-03). Here
`pairs` folds bytes through three `#[inline(never)]` functions: `split` (a `(u32, u32)`, a
pair), `widen` (a `(u8, u32)`, a pair with padding) and `long` (a `(u32, u64)`, wider than two
wasm32 pointers: its return place), called directly and through function pointers. The proc
macro `folded!` folds its input both ways in its `wasm32-wasip1` module; the program holds them
to `const`s the MIR evaluator folds, to the same fold as native code, and to the fold computed
here. Then the same again from nothing.
"""
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from blaze_cli import Scratch, base_env, write
from typed_input import fail, parse

SCRIPT = "wasmpairmacro.py"
WORDS = ("hello", "zed")

PAIRS = r'''#![no_std]

#[inline(never)]
pub const fn split(x: u32) -> (u32, u32) {
    (x.wrapping_mul(2654435761), x ^ 0x5555)
}

#[inline(never)]
pub const fn widen(x: u8) -> (u8, u32) {
    (x.wrapping_add(1), x as u32 * 1000)
}

#[inline(never)]
pub const fn long(x: u32) -> (u32, u64) {
    (x.rotate_left(5), ((x as u64) << 40) | 7)
}

macro_rules! fold_with {
    ($text:expr, $split:expr, $widen:expr, $long:expr) => {{
        let text = $text;
        let mut acc: u64 = 0xcbf29ce484222325;
        let mut i = 0;
        while i < text.len() {
            let byte = text[i];
            let (a, b) = $split((acc as u32) ^ byte as u32);
            let (c, d) = $widen(byte);
            let (e, f) = $long(a ^ b);
            acc = acc.wrapping_mul(0x100000001b3)
                ^ a as u64
                ^ ((b as u64) << 32)
                ^ c as u64
                ^ ((d as u64) << 8)
                ^ e as u64
                ^ f;
            i += 1;
        }
        acc
    }};
}

/// The fold with each function called directly: what a `const` is evaluated to.
pub const fn fold(text: &[u8]) -> u64 {
    fold_with!(text, split, widen, long)
}

/// The fold with each function called through a pointer: an indirect call of the signature.
pub fn fold_through(
    text: &[u8],
    split: fn(u32) -> (u32, u32),
    widen: fn(u8) -> (u8, u32),
    long: fn(u32) -> (u32, u64),
) -> u64 {
    fold_with!(text, split, widen, long)
}
'''

MACRO = r'''use proc_macro::TokenStream;
use std::hint::black_box;

/// `(direct, through)`: the input's text folded with direct calls and through pointers.
#[proc_macro]
pub fn folded(input: TokenStream) -> TokenStream {
    let text = input.to_string();
    let direct = pairs::fold(black_box(text.as_bytes()));
    let through = pairs::fold_through(
        text.as_bytes(),
        black_box(pairs::split as fn(u32) -> (u32, u32)),
        black_box(pairs::widen as fn(u8) -> (u8, u32)),
        black_box(pairs::long as fn(u32) -> (u32, u64)),
    );
    format!("({direct}u64, {through}u64)").parse().unwrap()
}
'''

MAIN = r'''const HELLO: u64 = pairs::fold(b"hello");
const ZED: u64 = pairs::fold(b"zed");

fn main() {
    let (hello, hello_through) = pairmac::folded!(hello);
    let (zed, zed_through) = pairmac::folded!(zed);
    let native = pairs::fold(std::hint::black_box(b"hello"));
    let native_through = pairs::fold_through(
        std::hint::black_box(b"zed"),
        std::hint::black_box(pairs::split as fn(u32) -> (u32, u32)),
        std::hint::black_box(pairs::widen as fn(u8) -> (u8, u32)),
        std::hint::black_box(pairs::long as fn(u32) -> (u32, u64)),
    );
    println!("module {hello} {hello_through} {zed} {zed_through}");
    println!("evaluator {HELLO} {HELLO} {ZED} {ZED}");
    println!("native {native} {native} {native_through} {native_through}");
}
'''

M32, M64 = (1 << 32) - 1, (1 << 64) - 1


def fold(text):
    """What `pairs::fold` answers of `text`, computed here."""
    acc = 0xCBF29CE484222325
    for byte in text.encode():
        x = (acc & M32) ^ byte
        a, b = x * 2654435761 & M32, x ^ 0x5555
        c, d = (byte + 1) & 0xFF, byte * 1000
        y = a ^ b
        e, f = ((y << 5) | (y >> 27)) & M32, (y << 40 | 7) & M64
        acc = (acc * 0x100000001B3 & M64) ^ a ^ (b << 32) ^ c ^ (d << 8) ^ e ^ f
    return acc


def expected():
    hello, zed = (fold(word) for word in WORDS)
    return (f"module {hello} {hello} {zed} {zed}\n"
            f"evaluator {hello} {hello} {zed} {zed}\n"
            f"native {hello} {hello} {zed} {zed}\n")


@dataclass(frozen=True)
class Input:
    keep: bool = False


def test_fold():
    """The fold here is the program's: its value for "" is the offset basis, and one byte
    moves every word of it."""
    assert fold("") == 0xCBF29CE484222325
    assert fold("a") != fold("b") and fold("hello") != fold("zed")


def main():
    match sys.argv[1:]:
        case []:
            args = Input()
        case [text]:
            args = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} [JSON] (see the top of the file)")
    # macOS: under /tmp, as a daemon's socket is under its home and macOS allows 104 bytes.
    # Elsewhere $TMPDIR: /tmp may be a tmpfs, which can't share blocks with blaze's cache.
    work = tempfile.mkdtemp(prefix="wasm-pair-macro-", dir="/tmp" if sys.platform == "darwin" else None)
    print(f"work: {work}", flush=True)
    at = os.path.join(work, "workspace")
    write(at, "Cargo.toml", '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\n'
          '[dependencies]\npairs = { path = "pairs" }\npairmac = { path = "pairmac" }\n'
          '[workspace]\nmembers = ["pairs", "pairmac"]\n')
    write(at, "src/main.rs", MAIN)
    write(at, "pairs/Cargo.toml", '[package]\nname = "pairs"\nversion = "0.1.0"\nedition = "2024"\n')
    write(at, "pairs/src/lib.rs", PAIRS)
    write(at, "pairmac/Cargo.toml", '[package]\nname = "pairmac"\nversion = "0.1.0"\nedition = "2024"\n'
          '[lib]\nproc-macro = true\n[dependencies]\npairs = { path = "../pairs" }\n')
    write(at, "pairmac/src/lib.rs", MACRO)
    want = expected()
    passed = False
    base = None

    def run(command):
        ran = subprocess.run(command, cwd=at, env=base, capture_output=True, text=True, timeout=900, check=False)
        assert ran.returncode == 0, f"{command}: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        return ran.stdout

    try:
        base = base_env(os.path.join(work, "home"))
        ours = run(["blaze", "run", "-q"])
        assert ours == want, f"the module, the evaluator and native code against the fold here:\n{ours}!=\n{want}"
        _, ran = Scratch(at, base).cargo("run", "-q", timeout=900)
        assert ran.returncode == 0, f"from nothing: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        assert ran.stdout == want, f"from nothing:\n{ran.stdout}!=\n{want}"
        print(f"  {ours.splitlines()[0]}: the evaluator's, native code's and the fold here, and so from nothing",
              flush=True)
        run(["blaze", "stop"])
        passed = True
    finally:
        if base:
            subprocess.run(["blaze", "stop"], cwd=at, env=base, capture_output=True, check=False)
        if passed and not args.keep:
            shutil.rmtree(work)
    print("PASS: a proc macro's pair returns, as WebAssembly, are the MIR evaluator's", flush=True)


if __name__ == "__main__":
    test_fold()
    main()
