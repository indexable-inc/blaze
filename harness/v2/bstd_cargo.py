"""bstd_cargo.py: bstd under cargo. bstd (crates/blaze-std) is written for blaze: contract clauses
(`-> (r: T) ensures ..`), `where const { .. }` conditions, const generic expressions and
`core::reflect`, which the pinned nightly's rustc refuses, so `cargo check -p blaze-std` fails
(agents/annoyances/fallible-1.json, decision D25). While cargo is the dev-loop builder this
writes a copy of the crate with that syntax taken out, as the package `blaze-std-shim` in
`crates/zz-bstd-shim`, and runs cargo on it:

    uv run --frozen python harness/v2/bstd_cargo.py '{"command": "check"}'
    uv run --frozen python harness/v2/bstd_cargo.py '{"command": "clippy"}'
    uv run --frozen python harness/v2/bstd_cargo.py '{"command": "test"}'
    uv run --frozen python harness/v2/bstd_cargo.py '{"command": "write"}'

The input is one JSON object:

    command   write    only writes the copy
              check    the copy, then `cargo check -p blaze-std-shim --all-targets`
              clippy   the copy, then `cargo clippy -p blaze-std-shim --lib --no-deps`, which
                       holds bstd's own lints (crates/blaze-std/clippy.toml, the D29 denies)
              test     the copy, then `cargo test -p blaze-std-shim --no-fail-fast`
    root      the checkout (".")
    filter    a test name filter for `test` ("")

What the copy is not: contracts are not checked; the ownership walk (`bstd::own`) answers
nothing, so its tests (`tests::own`) and those of what it derives (`tests::cell`) are left
out; a memo point's code identity is a hash of its type name. A green run here is evidence
about the rest of bstd under rustc, never about bstd under blaze: `blaze check` and `blaze
test` of bstd stay the proof (rule blaze-builds-blaze). The copy is never committed: add
`crates/zz-bstd-shim` to `.git/info/exclude`, and do not commit the line cargo adds to
`Cargo.lock`. Edit crates/blaze-std, never the copy.

A substitution that no longer finds its text stops the script, naming the file: bstd changed
there and the stand-in has to follow.
"""

import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import parse

SCRIPT = "bstd_cargo.py"


@dataclass(frozen=True)
class Input:
    command: Literal["write", "check", "clippy", "test"]
    root: str = "."
    filter: str = ""


def write(root: Path) -> Path:
    """The copy of crates/blaze-std that rustc accepts."""
    src, dst = root / "crates/blaze-std", root / "crates/zz-bstd-shim"
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)

    def edit(rel, fn):
        p = dst/rel; s = p.read_text(); t = fn(s)
        if s == t:
            sys.exit(f"{SCRIPT}: the stand-in for {rel} no longer finds its text: bstd changed there; change the substitution in this script to match")
        p.write_text(t)
    edit('Cargo.toml', lambda s: s.replace('name = "blaze-std"', 'name = "blaze-std-shim"'))
    edit('src/lib.rs', lambda s: re.sub(r'\n    (blaze_comptime|blaze_const_exprs|blaze_verify),', '', s))
    def contracts(s):
        out, lines, i = [], s.split('\n'), 0
        while i < len(lines):
            m = re.match(r'^(.*-> )\(r: (.*)\)$', lines[i])
            if m and i + 1 < len(lines) and lines[i+1].strip().startswith(('ensures', 'requires')):
                j = i + 1
                while lines[j] != '    {': j += 1
                out.append(m.group(1) + m.group(2) + ' {')
                i = j + 1
                continue
            out.append(lines[i]); i += 1
        return '\n'.join(out)
    for rel in ['src/budget.rs', 'src/clock.rs', 'src/fs.rs', 'src/kv.rs']:
        edit(rel, contracts)
    edit('src/own.rs', lambda s: re.sub(r' where const \{ none::<T, \w+>\(\) \}', '', s))
    edit('src/own/raw.rs', lambda s: s.replace('core::reflect::name_of(id)', '{ let _ = id; "?" }'))
    edit('src/eval.rs', lambda s: s.replace('pub use core::reflect::MemoPoint;', 'pub trait MemoPoint {}\nimpl<F> MemoPoint for F {}')
         .replace('const { core::reflect::code_id_of::<F>() }', '{ let mut id = [0u8; 32]; let mut h: u64 = 0xcbf29ce484222325; for (at, byte) in core::any::type_name::<F>().bytes().enumerate() { h = (h ^ byte as u64).wrapping_mul(0x100000001b3); if let Some(slot) = id.get_mut(at % 32) { *slot ^= (h >> 24) as u8; } } id }'))
    def raw(s):
        # Reflection is blaze's: under cargo each fact is a stub, so the walk type-checks but
        # answers nothing (the own tests are not run under the shim).
        stubs = {
            'shape': 'Shape::Unseen', 'variants': '1', 'fields': '0', 'field': 'id',
            'field_name': '"?"', 'variant_name': '"?"', 'inner': 'None', 'args': '0', 'arg': 'None',
            'implements': 'false', 'name': '"?"',
        }
        for name, value in stubs.items():
            m = re.search(r'(pub const fn %s\(([^)]*)\) -> [^{]+\{)\n(.*?)\n\}\n' % name, s, re.S)
            assert m, name
            args = [a.split(':')[0].strip() for a in m.group(2).split(',') if a.strip()]
            body = '    let _ = (%s);\n    %s' % (', '.join(args), value)
            s = s[:m.start()] + m.group(1) + '\n' + body + '\n}\n' + s[m.end():]
        return s.replace('use core::mem::type_info::{self, Generic, TypeKind};', 'use core::mem::type_info;')
    edit('src/own/raw.rs', raw)
    edit('src/own/shares.rs', lambda s: re.sub(r'\n    where\n        const \{[^\n]*\},\n', '\n', s).replace('{ N + M }', 'N').replace('    pub fn join<const M: usize>', '    #[allow(clippy::needless_pass_by_value)]\n    pub fn join<const M: usize>'))
    edit('src/own.rs', lambda s: re.sub(r'\nunsafe impl<[^\n]*A: Allocator \+ Clone> Owns<Solely> for BTree(Map|Set)<[^\n]*\n', '\n', s))
    # The tests of reflection (own) and of what it derives (cell's Send) need blaze.
    edit('src/tests.rs', lambda s: s.replace('\nmod own;', '').replace('\nmod cell;', ''))
    return dst


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    dst = write(root)
    print(f"{SCRIPT}: wrote {dst.relative_to(root)}")
    if it.command == "write":
        return
    toolchain = (root / "harness/toolchain").read_text().strip()
    cargo = ["cargo", f"+{toolchain}"]
    package = ["-p", "blaze-std-shim"]
    args = {
        "check": ["check", *package, "--all-targets"],
        "clippy": ["clippy", *package, "--lib", "--no-deps"],
        "test": ["test", *package, "--no-fail-fast", *(["--", it.filter] if it.filter else [])],
    }[it.command]
    sys.exit(subprocess.run([*cargo, *args], cwd=root).returncode)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
