#!/usr/bin/env python3
"""Native proc macros answer source queries across edits and restarts as from nothing (#823).

The same derive asks for line/column, source text, byte range and file. Under one daemon, both
the native opt-out and the default wasm path must print what `blaze run` of the same source
prints from nothing (a home, store and target directory of its own). Moving and renaming the
item, then restarting with the expansion store intact, must not reuse an old answer.
"""
import os, shutil, subprocess, tempfile

from blaze_cli import Scratch, base_env, write

MACRO = r'''#![feature(proc_macro_span)]
use proc_macro::{TokenStream, TokenTree};
#[proc_macro_derive(Where)]
pub fn where_(input: TokenStream) -> TokenStream {
    let name = input.into_iter().skip_while(|t| t.to_string() != "struct").nth(1).unwrap();
    let TokenTree::Ident(name) = name else { panic!("expected a name") };
    let span = name.span();
    let text = span.source_text().unwrap();
    let range = span.byte_range();
    let file = span.file();
    let local = span.local_file().unwrap();
    format!("impl {name} {{ fn place() {{ println!(\"{{}} {{}} {{}} {{}} {{}} {{}} {{}}\", {}, {}, {:?}, {}, {}, {:?}, {:?}); }} }}",
            span.line(), span.column(), text, range.start, range.end,
            file.ends_with("src/main.rs"), local.ends_with("src/main.rs"))
        .parse().unwrap()
}
'''


def main():
    work = tempfile.mkdtemp(prefix="native-spans-", dir="/tmp" if os.name != "nt" else None)
    print(f"work: {work}", flush=True)
    at = os.path.join(work, "workspace")
    write(at, "Cargo.toml", '[package]\nname = "span-use"\nversion = "0.1.0"\nedition = "2021"\n'
          '[dependencies]\nasker = { path = "asker" }\n[workspace]\n')
    write(at, "asker/Cargo.toml", '[package]\nname = "asker"\nversion = "0.1.0"\nedition = "2021"\n'
          '[lib]\nproc-macro = true\n')
    write(at, "asker/src/lib.rs", MACRO)
    passed = False
    base = None

    def run(command):
        ran = subprocess.run(command, cwd=at, env=base, capture_output=True, text=True, timeout=600)
        assert ran.returncode == 0, f"{command}: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        return ran.stdout

    def from_scratch():
        _, ran = Scratch(at, base).cargo("run", "-q")
        assert ran.returncode == 0, f"blaze run from nothing: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        return ran.stdout

    try:
        for mode in ("0", "1"):
            base = base_env(os.path.join(work, "home-" + mode))
            base["BLAZE_PROC_MACRO_WASM"] = mode
            # No opt-in to the bidirectional protocol is supplied by the user.
            base.pop("RUST_ANALYZER_USE_POSTCARD", None)
            for lines, name in ((5, "Here"), (93, "Here"), (17, "Renamed")):
                source = "\n" * lines + f"#[derive(asker::Where)]\nstruct {name};\nfn main() {{ {name}::place(); }}\n"
                write(at, "src/main.rs", source)
                expected = from_scratch()
                ours = run(["blaze", "run", "-q"])
                assert ours == expected, f"mode {mode}, line {lines}, {name}: {ours!r} != {expected!r}"
                print(f"  wasm={mode}, {name} after {lines} lines: {ours.strip()}", flush=True)
            run(["blaze", "stop"])
            ours = run(["blaze", "run", "-q"])
            assert ours == expected, f"mode {mode}, kept store: {ours!r} != {expected!r}"
            print(f"  wasm={mode}, restart with store: as from nothing", flush=True)
            run(["blaze", "stop"])
        passed = True
    finally:
        if base:
            subprocess.run(["blaze", "stop"], cwd=at, env=base, capture_output=True)
        if passed:
            shutil.rmtree(work)
    print("PASS: native and wasm source queries, edits and kept store", flush=True)


if __name__ == "__main__":
    main()
