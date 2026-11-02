---
status: plan
summary: How blaze v2 reads build scripts (what they print, their environment, the scripts elision reads instead of running, what reruns them) in one pass over borrowed bytes, and how each part compares with the fastest known way.
date: 2026-10-29
---
# Build scripts

Part of [usercode](../usercode.md). Code: `crates/blaze-build-script/`. Requests:
`v2/requests/usercode-fast-scripts.md`.

Every part below reads its input once, borrowed, and allocates only what it hands back. Each
rewrite keeps the code it replaced as a test-only reference (`tests/reference.rs` beside its tests) and a
seeded test that the two agree on every input: what changed is speed, not what is read.
Nothing here is measured yet: the comparisons are of technique, and the benchmark to settle
them is under [What is not done](#what-is-not-done).

## What a script prints (`output.rs`)

cargo's `BuildOutput::parse` (`src/core/compiler/custom_build.rs`), the baseline, splits the
output on `\n` a byte at a time, reads each line as text, and matches each key with a string
`match` (a chain of comparisons, one per key until one agrees), owning every value it keeps.

blaze reads it in one pass ([`output::read`]): line breaks are found by memchr (SIMD, 16 or
32 bytes a step), a line that does not start `cargo:` is passed over without being read as
text, and the key is matched by its length and then its bytes into a `u8` enum (`Key`), so no
key costs more than one comparison of its own length. Each directive goes to a `Sink` as a
`Directive<'_>` whose values borrow the output; a value is copied only when it names the
`OUT_DIR` the script ran with and that directory moved (found by a `memmem::Finder` built
once per output). `parse` is the sink that keeps an `Output`; `watch::watched_in` keeps the
`rerun-if-*` values alone and copies nothing else.

The bounds, with their arithmetic, are in [speed-of-light.md](speed-of-light.md). This
matches the fastest known way for this input. Lines are short and few (tens to
thousands), so memchr's line splitting is the bound, and the dispatch is one jump on length
plus one compare. What is left is the UTF-8 check of each directive line, which std does in
SIMD already.

## The environment (`env.rs`)

cargo builds the environment in a `ProcessBuilder`'s `BTreeMap` of owned strings: a node
and a key allocated per variable, and a `format!` per derived value.

blaze presizes one vector for what cargo sets; builds each value at its exact length without
`format!` (`prefixed`, `decimal`, `concat`); takes the fixed names as `&'static str`; sorts
what cargo sets once, by a big-endian `u128` of each name's first sixteen bytes, then the
name, then the order set, keeping the last word per name; and merges it with the inherited
environment, which is sorted only when the caller did not pass it sorted, copying an
inherited variable only where it is kept. `Env::vars` is the sorted vector; lookups (`get`,
`origin`) are binary searches.

One allocation per kept string, one sort of about a hundred entries whose comparisons are
mostly one integer compare, and one merge. A reused buffer would only add a copy, since every
kept string is handed back owned; the one place that joins scratch paths
(`cargo_started_in`) reuses a `PathBuf`. The bound and the gap left are in
[speed-of-light.md](speed-of-light.md#the-environment-env).

## Scripts read rather than run (`elide.rs`, `elide/lex.rs`)

Relax (`front/src/unitgraph_noop.rs` of makepad/makepad, #1296) decides a script prints only
constants by matching its source text against the forms it reads. simdjson's stage 1 is the
fastest known lexer for small grammars: classify 64 bytes at a time into bitmasks (quotes,
backslashes, structure, whitespace), then walk the set bits.

blaze lexes with a 256-entry byte-class table (built by a `const fn`) and jumps over the
inside of strings, block comments and raw strings with memchr2 and memchr, so the per-byte
work is one table load except inside literals, where it is SIMD. Most scripts start with
`use`, and are refused at their first bytes (`may_open_main`), before the file is validated,
normalized or lexed. Past that, tokens are made on demand (`Tokens`), and elision asks for
them only as its forms need them. A string token keeps
its text and whether it has escapes; its value is decoded only when the script is printed
(`Token::push_value`), into a buffer reused for every call, so no token allocates.

Where this falls short of simdjson: stage 1's bitmasks would classify a whole file in a few
instructions per 64 bytes, where we take one load per byte outside literals. For a `build.rs`
of a few kilobytes that is a few microseconds, below the cost of reading the file, and the
early exit means most files are not lexed past their first line. Closing the gap would need
`std::simd` (nightly-only, portable) or `unsafe` intrinsics, which this crate forbids; it is
worth doing only if a profile shows lexing, not I/O, on the path.

Against Relax: both refuse at the first form not read; Relax matches text and so refuses
scripts a comment or a line break moves, where tokens read every spelling rustc reads.

## What reruns a script (`watch.rs`, `apply.rs`)

cargo joins each `rerun-if-changed` path to the package's directory with a fresh `PathBuf`
and dedups by a linear scan; `mtime_recursive` stats each entry by its full path.

blaze joins every path in one reused `PathBuf` and copies it out only when it was not met
before; dedup is a scan up to 16 entries and a hash of each value past that (a script naming
every file it reads lists hundreds). `Rerun::stale` builds each path in the thread's path
buffer (`blaze_mem::scratch::with_path`), and compares variables borrowed. The recursive walk reads each entry's
own time from its directory (`DirEntry::metadata`, which is `fstatat` on Unix) and makes its full path only for a link or a directory. `to_link`
(cargo's `build_map`) asks the graph for each dependency's node once per edge where it asked
twice, and does not follow a dependency that is neither for the host nor linked, which can
give nothing.

## Runs (`run.rs`)

`run`, `elided`, `begin` and `Ran::finish` make directories, write cargo's files and stat
them: they block, and are for the harness and v1. A caller on a thread that must not block
(blaze v2's daemon, which acts through effects) uses `run::judge`, which reads only the
bytes it is given, and makes the directories and files itself from `run::invoked` and
`run::files`. What a failed run shows (`logged`) is found by one `memmem` search for
`\ncargo:`, so lines that are not directives are never looked at.

## The standard library's own scripts (`blaze_units::stdlib::scripts`)

Decided 2026-10-31 (queue 01244, lane std-scripts for cargo). std's packages with build
scripts in a hello-world build are `compiler_builtins`, `libc`, `object` and `std`. They
can't be compiled the way a user's script is: blaze has no sysroot, and the host std a script
would build against is built from the same `library/`, whose units need these scripts' output
first. `merge_std` (`crates/blaze-units/src/load/units.rs`) gives std's own units no std edge,
so each compile failed with `Fn build_script_build::main has no hash: MIR that failed to
lower`.

What each script calls, read in the pinned nightly's `library/` (nightly-2026-10-27):

| script | reads | starts or touches |
|---|---|---|
| `std/build.rs` | `CARGO_CFG_TARGET_{ARCH,OS,VENDOR,ENV}`, `RUSTC_STD_NETBSD10`, `RUSTC_BOOTSTRAP_SYNTHETIC_TARGET`, `WIND_RELEASE_ID` | nothing |
| `vendor/object-0.39.1/build.rs` | `RUSTC` | `$RUSTC --version` |
| `vendor/libc-0.2.191/build.rs` | `RUSTC`, `RUSTC_WRAPPER`, `LIBC_*`, `CARGO_CFG_*`, `RUST_LIBC_UNSTABLE_*` | `$RUSTC --version`; `freebsd-version` under `LIBC_CI`; `emcc -dumpversion` for emscripten |
| `compiler-builtins/compiler-builtins/build.rs` with `libm/configure.rs` | `TARGET`, `OPT_LEVEL`, `CARGO_*`, `LIBM_BUILD_VERBOSE`, `ENSURE_NO_PANIC`, the whole environment (`env::vars()`) | `env::current_dir()`; the `cc` crate under the `c` feature |

Two designs were weighed. In the first, the scripts' units would get an edge to a host std graph
lowered with no script outputs (as `probe::std_script` reads std), and the pure-scripts
interpreter would evaluate them. The evidence rules it out. `hir_ty_pure` proves purity; it
evaluates nothing, and a pure script still compiles and runs on a miss
([pure-scripts design](../pure-scripts/design.md) section 7). By its own rules all four are
impure: `object` and `libc` start a process, `compiler_builtins` lists the environment and
reads the working directory, and `std` reads variables outside `allowed_env`. So every hello
build would stop at a typed refusal. To make that design work, blaze would first need a MIR
evaluator for build scripts over a second, host-lowered std, plus models for `Command` and
`env::vars`, all just to print about 200 constant lines.

The second design is the one taken: each script is ported, pinned by the BLAKE3 hash of every file
it is made of (`PINNED`). `identify` checks the files
through the VFS, and `printed` writes what the script prints, line for line and in its order,
from the run's own environment (`blaze_build_script::env`, as cargo starts it). A script whose
bytes differ, or a std package whose script has no port, is an error naming the file, its hash
and the pinned one: never run, never guessed (no-fallback). Where a script starts a program,
the port answers as blaze's `rustc` stand-in does (`rustc <target::rustc_version>`). A program
blaze has no answer of (`emcc`, `freebsd-version`, a set `RUSTC_WRAPPER`) and compiling
compiler-rt's C are errors naming them. Where the script would panic, the port's error says
why.

In the daemon (`crates/blaze-cargo/src/serve/unit/script.rs`), a script unit that `is_std` is
`ported`. Its output takes the inert path: the compile builds nothing, the run is not started,
and its `OUT_DIR` is empty, which is what each of the four leaves. `BLAZE_ELIDE=verify` and
`BLAZE_PURE=verify` don't apply, since there is nothing to compile. None of the four reads a
`DEP_*` variable, so the output is known before the runs it depends on end.

The cost is that the ports are a table to keep current. When a nightly bump changes a script,
the bump fails at `every_pinned_file_is_the_checkouts`
(`crates/blaze-units/src/stdlib/scripts/tests.rs`) before any build does, and the script is
ported again. std's vendored scripts change a few times a year. blaze's rustc stand-in reads the
same port: `probe::std_script` (`crates/blaze-engine/src/as_rustc/probe.rs`) identifies std's
script and sets the cfgs and variables `printed` gives for the probe's target, so a bump ports
std's script once.

Request 00710 said std's build-script units are host units with no edge to std roots, and that
how one links a std is the daemon's. The first part still holds: `merge_std` is unchanged, and
`std_scripts_are_std_and_need_no_std` checks it. The second part is replaced: no std is linked
for these scripts, because they are never compiled. The cache keys miss nothing. The output is a
function of the pinned files, which are checked by hash every job, of the run's environment,
which is read whole, and of blaze's rustc version, which is in every key through the toolchain.

## What is not done

- A benchmark: the seeded generators of the differential tests at sizes from real crates
  (the corpus of `harness/scripts.py`), old against new, per part. Until then the claims
  above are of technique.
- The callers' side, which is other lanes' (`v2/requests/usercode-fast-scripts.md`): blaze-
  cargo judging runs by borrowing (`run::judge`) rather than cloning both outputs, making
  `OUT_DIR`, the run directory and cargo's files through effects, and blaze-usercode reading
  what reruns a script with `watch::watched_in` rather than parsing the whole output.

[`output::read`]: ../../../../../crates/blaze-build-script/src/output.rs
