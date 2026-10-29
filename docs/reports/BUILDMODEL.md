---
status: record
summary: Cargo's build model in blaze: units, profiles and plans.
date: 2026-10-20
---
# Cargo's build model in blaze

Branch `blaze-build-model`. The head has `blaze-collate-next` 1b022ff6f2 merged in (before
it `blaze-collate` 3d47ed59bf and 14ee89181b; the third round from 1547078a30, the second
from 0151c01218). Work stopped here; "Where this stands" below says what is done, what is
half done and what would come next. Linux x86-64, Rust 1.99.0,
LLVM 20.

This is a survey of which parts of cargo's build model blaze follows, with a small project
for each part under `harness/buildmodel/` and a script, `harness/buildmodel.py`, that builds
every one with cargo and rustc alone and again with blaze's code linked in, runs both and
compares what they print and return. Where a case has steps, one input is changed at a time
under the same daemon and each step is compared with cargo again.

## Result

Measured with

    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p blaze-engine --release --features llvm
    cd harness
    BLAZE_BIN=... python3 buildmodel.py --backend cranelift --jobs 6 --table table-cranelift.md
    BLAZE_BIN=... BLAZE_OPT=2 BLAZE_EMIT=fast python3 buildmodel.py --backend llvm --jobs 6 --table table-llvm.md

Result: 105 of 105 as expected under Cranelift and 105 of 105 under LLVM, 0 WRONG. The two
tables are the same but for the backend column: 100 supported and 5 refused. Of the 5, 2 are
projects cargo itself turns down and 3 are `lto` with rustc's own LTO link line (in a
workspace, fat and thin, and in a program of one crate). The cases have 36 invalidation
steps between them that cargo has built for, and all 36 come out the same as cargo's
rebuild. The case `overflow_checks_stale` has 4 steps more, of which the 2 asked with what
cargo linked before an edit are refused, as they should be, and the 2 after cargo has built
are the same. The third round ended at 100 cases; the fourth adds the two `lto` rows of a
program of one crate, doctests of two members of a workspace, doctests with `--target`, the
stale overflow evidence, and outside the
table the doctests of 30 crates from crates.io, 30 of 30 the same as `cargo test --doc`
under each backend.

The second round ended at 97 cases per backend, 92 supported and 5 refused, with 31 steps of
which 2 were refused. The first ended at 90 cases, 82 supported and 8 refused, with 16 steps
of which 7 were refused.

A row is one of three things. "supported": the two programs print and return the same, and
every function of the crates the case names was compiled by blaze. "refused": blaze said why
it does not build this and nothing ran differently; that includes a function it left to
rustc with a reason. "WRONG": a different output, a crash, or a refusal without a reason. The
script fails on a WRONG row and on any row that is not what its `case.json` expects, so a
feature that stops working, or a refusal that starts to pass, is seen.

## Fourth round

### The merge

`blaze-collate` is merged in four steps, as it moved: 2da3d6a792 (890d2ee757, conflicts in
`Cargo.lock` and `crates/blaze-engine/src/main.rs`), then ce52276cf5 with the platform ABI
unification and the third S3 round (a6b6b5ede8, one conflict in
`crates/hir-ty/src/mir/codegen.rs`, where each side had added a field to `ObjectCodegen`;
both are kept), then 8f21942505 with the fuzzer (9cf4286511, no conflict), then f191c6e42a, which only adds
the two files under `publish/` (34d3333c9d, no conflict), then 14ee89181b with the security
round, the second round of the cargo drop-in, the docs and the publish tooling (1b4047e6f4,
conflicts in ten files, see below). In `main.rs` the
third round's widening is kept over the second round's refusal as it had been resolved in
`blaze-collate`; `Cargo.lock` is `blaze-collate`'s, which has the dependencies of the remote
store. A unit test of `cargo/cli.rs` that still expected `--release`, a custom profile and
feature flags to be left to cargo was brought up to the one plan.

### Doctests of 30 crates

`harness/doctests.py` now runs `cargo test --doc` and `blaze test --doc` on 30 crates
from crates.io and compares the exit status and everything printed (test lines, failure
blocks with the compiler's errors, the result line), with only the order of tests, the
times and thread numbers taken out:

    cd harness
    BLAZE_BIN=... BLAZE_BACKEND=cranelift python3 doctests.py
    BLAZE_BIN=... BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast python3 doctests.py

Result on the head of the branch: 30 of 30 the same under Cranelift and 30 of 30 under LLVM,
none left to rustdoc, 0 WRONG. Together they have 3095 doctests, and blaze compiled 1739 of
the 14292 functions of the libraries; the rest are generic or inlined and are rustc's code
in the example, as said in the third round.

| crate | doctests | functions of the library from blaze |
|---|---|---|
| semver 1.0.28 | 4 | 43 of 107 |
| either 1.19.0 | 61 | 2 of 172 |
| smallvec 1.16.3 | 14 | 2 of 170 |
| serde_core 1.0.229 | 68 | 109 of 2217 |
| regex 1.13.1 | 247 | 134 of 363 |
| itertools 0.15.0 | 210 | 11 of 910 |
| chrono 0.4.45 | 280 | 374 of 982 |
| anyhow 1.0.104 | 26 | 27 of 117 |
| bytes 1.12.1 | 248 | 128 of 563 |
| thiserror 2.0.21 | 10 | 2 of 9 |
| log 0.4.34 | 26 | 31 of 113 |
| bitflags 2.13.2 | 21 | 14 of 53 |
| indexmap 2.14.2 | 41 | 4 of 793 |
| once_cell 1.21.4 | 43 | 3 of 112 |
| hashbrown 0.17.1 | 275 | 5 of 692 |
| memchr 2.8.3 | 25 | 73 of 513 |
| url 2.5.8 | 67 | 122 of 235 |
| uuid 1.28.0 | 70 | 122 of 275 |
| time 0.3.55 | 410 | 11 of 878 |
| rand 0.10.3 | 61 | 27 of 408 |
| arrayvec 0.7.8 | 43 | 1 of 154 |
| base64 0.23.1 | 26 | 60 of 149 |
| byteorder 1.5.0 | 122 | 2 of 73 |
| num-traits 0.2.19 | 184 | 3 of 2150 |
| strum 0.28.0 | 4 | 2 of 6 |
| nom 8.0.0 | 324 | 69 of 720 |
| textwrap 0.16.4 | 59 | 43 of 79 |
| unicode-segmentation 1.13.3 | 23 | 21 of 161 |
| httparse 1.10.1 | 6 | 26 of 116 |
| serde_json 1.0.154 | 97 | 268 of 1002 |

The numbers are the same under both backends.

`serde` itself has no doctests since its code moved to `serde_core`, so that is what is
run. In `serde_core`, `chrono` and `strum` some doctests fail under cargo too when the
crate is tested alone out of its workspace (18, 3 and 4 of them); blaze's run fails the
same ones with the same compiler errors.

The first run of the list had many of them different. What was wrong, all of it in how the
examples are found and named and none of it in the code blaze compiled:

- rustdoc's own lint flags (`--allow`, `--warn`, ..) made blaze leave the crate to rustdoc.
  They say nothing of how an example is built and are now passed over.
- An example that does not name the library is not linked with it. blaze put its object of
  the library on that link all the same, and its undefined references failed the link
  (`anyhow`, `memchr`). blaze's object now goes only on a link that has the library.
- The line of an example is counted from the first comment or attribute of the
  documentation, also for `/*! */`, for a `doc` in a `cfg_attr`, and for what a macro
  wrote, where rustdoc has the line of the macro's use.
- Documentation that is only there with `cfg(doc)` or `cfg(doctest)` was not seen. The
  crate is now read again with both set after its code is built without them.
- A `macro_rules!` macro is named by the module it is written in (`macros::bail`), not by
  the root it is exported to.
- The type of an `impl` is named as the compiler prints it: `&'_mutDeserializer<R>`.
- Documentation on an `extern` block is read.
- In a crate of edition 2024 rustdoc has the examples of a file read with `include_str!`
  be in that file, by its lines (`src/../README.md - ReadmeDoctests (line 52)`); before 2024 they
  are in the file of the attribute. With a comment beside the `include_str!` it is the
  comment's line that is counted from. All of that was taken from rustdoc on a small crate
  and is what blaze does now.
- In edition 2024 rustdoc names a `should_panic` example ` - should panic`.

The harness had a fault of its own: the last failure block was compared together with the
list of failures after it, so two runs with the same failures in another order differed.

### `lto` and a link command

A daemon that is given a link command for a program with `lto` in its profile now looks in
it for `core` as a library (`libcore-*.rlib`, or std's shared library). rustc's LTO link has
neither, also for a program of one crate, and blaze refuses with the sentence. The rows
`lto_single` and `lto_single_linked` hand blaze the link line (`"handed"` in `case.json`):
with the line of a build with `lto = "off"` the program is the same as cargo's, with rustc's
LTO line blaze refuses. What is left is the client that links by itself (`--out` only, as
`buildmodel.py` does for its other rows): blaze never sees that link and cannot say anything
of it, and the linker's undefined symbols are what that client gets.

### Doctests of a workspace and of another target

`doctest_members` runs `cargo test --doc -p app -p util` where `app` uses `util` (8
examples, 3 of the 4 library functions from blaze; the fourth is generic). `doctest_cross`
runs the doctests with `--target aarch64-unknown-linux-gnu` under qemu, with examples
marked `ignore-aarch64` and `ignore-x86_64` (4 examples, 2 of 2 functions). Both are the
same as cargo's under both backends.

### Overflow checks and what rustc built before an edit

In a program whose crates differ in `overflow-checks`, blaze reads from rustc's objects
which crate holds each copy of a function. That is only true of the source rustc built. The
case `overflow_checks_stale` has a function of a package that checks, called from one that
does not; an edit makes it small enough for rustc to compile it in its caller, where it
stops checking. A client that asked the daemon with what cargo had linked before the edit
got a program that still panicked where cargo's printed 0. Two things were wrong and both
are fixed:

- The daemon went by the objects of before the edit. It now refuses in such a program when
  a source that cargo's dep-info lists for a crate is newer than what rustc built of the
  crate, and says that cargo has to build first. Programs whose crates all check alike are
  not affected: every copy behaves as every other. A file a build script wrote under
  `OUT_DIR` is compared by what is in it, not by when it was written: cargo runs a script
  again without building the crate again when the script says the same, and going by the
  time refused a `keys.py` case that was right.
- After cargo had built again and the client named the new link, the daemon still had the
  symbols it read when it started. In such a program it now reads them again when the files
  are newer than its reading, and compiles the program from them.

The case has four steps under each backend: refused, the same once cargo has built, refused
after the edit back, the same again. The steps that ask with the link of before are marked
`"stale"` in `case.json`; every other step of the harness has cargo build first, which is
why this was not seen before.

### The merge of 14ee89181b

The second round of the cargo drop-in and this branch had both taken release and custom
profiles, feature flags and `RUSTFLAGS` away from cargo, each in its own way. What is kept:

- `plan::Selection` stays the one notion of which build is meant. The drop-in's separate
  fields of a worker (`features`, `packages`, `workspace`, `profile` and the rest) are
  dropped for it, and `Invocation::selection` now carries the packages named with `-p` and
  `--workspace`, which the drop-in had started to hand to the daemon.
- `cargo/plan.rs` keeps this branch's rule that a crate built with LTO is cargo's when
  `blaze` runs cargo itself; the drop-in's new rule for test harnesses is kept as it
  came.
- The key of a store entry has both additions: each field written with its length (the
  security round) and the program an entry is of when crates differ in `overflow-checks`
  (this branch). The identity of such a program is made from the crates' content ids, which
  the security round made 32 bytes; the tag of the key moved to `blaze entry 8`.
- The drop-in now gives every cargo it runs the wrapper's variables, so that the cargo of
  `blaze test --doc` does not build the workspace a second time. blaze in rustdoc's
  place saw those variables and took itself for the rustc wrapper; every doc test run ended
  in "No such file or directory". `main` now tells the two apart by what cargo gives each:
  rustc's path first to the wrapper, flags alone to rustdoc. `blaze_cli.py` caught it (5
  flag cases under each of cold, warm and edit).
- `blaze_cli.py` has both sides' cases. The program of its workspace prints three lines
  for this branch's cases, so the checks of the daemon's life that compared its whole output
  with one line now compare the first line.

`selects.py` failed once in CI on `blaze-collate` (run 39086041463, LLVM leg) with two
markers on one line: "build script of devdepbuild script of dep". The scripts wrote their
line with `writeln!` to a file opened for appending, which is two writes, the text and then
the newline (seen with `strace`), and cargo runs build scripts at the same time. The line
is now written in one write. The selection itself was right.

## Third round

Starts from `blaze-collate` at 1547078a30 merged in (c41ee3294c).

### 1. Overflow checks where rustc picks the copy

Fixtures first. `harness/keys/kdep` got a small plain function, `klib` and `kapp` both use
the same instantiation of a generic function of `kdep`, and three cases were added to
`keys.py`: overflow checks of the program alone in release, of all but the package a small
function is written in in release, and of all but the package whose copy of generic code the
program uses in dev. Before the fix one of the three came out different from rustc (the
small function in release); the dev case was right by luck.

What rustc does, measured on a four-crate workspace by reading which crate's object holds
each instance:

- A check is kept when the crate the function is written in has overflow checks on and the
  crate that compiles the copy has them on too.
- In dev (`opt-level` 0, shared generics) a generic instance is compiled by the most
  upstream crate that uses it, and crates further down use that copy. `#[inline]` functions
  get one copy per crate that uses them.
- In release nothing is shared: generic and `#[inline]` code is compiled by the crate that
  uses it, and so is a small plain leaf function, which rustc decides by itself to make
  inlinable across crates.

What blaze does now. blaze keeps one copy per instance and setting, rustc one per crate, so
blaze has to know which crate's copy a call would have reached. The v0 symbol of a copy ends
with the crate that instantiated it, and blaze reads that from rustc's objects
(`symbols.rs`, `codegen/settled.rs`):

- When the instance has a name blaze can spell and rustc's objects hold a copy of it, the
  call takes the setting of the crate whose object holds the copy the asking crate links to.
- A plain function with no symbol exported from the crate it is written in was compiled into
  its users, so it takes the asking crate's setting. That is the release case.
- When blaze cannot tell, the function that makes the call is refused and left to rustc,
  with the reason in the report. That happens when the instance has no name blaze can spell
  and a crate with a different setting made a copy of a function of that name (for a
  closure, of the function it is written in), or when two crates with the same setting
  would reach copies that check differently.

I chose "observe, else refuse" over "refuse every program with mixed settings" because the
first is right for everything it builds and still builds most of it: in the `keys.py`
fixture 8 functions over the mixed cases are refused and the rest are blaze's.

A program whose crates differ in overflow checks has its settings in every store key
(`Entry::key`, namespace "blaze entry 7"), since what a call resolves to is no longer a
function of the caller's name alone.

Not closed: the evidence is rustc's last build. An edit that adds a new instantiation, or
moves a function across rustc's inlining threshold, in a program with mixed settings is
judged by the old objects until cargo builds again. And generic code of std that std ships
a copy of was not examined, in programs with one setting either.

### 2. A request in the middle of a reload

`BLAZE_RELOAD_PAUSE_MS` makes the daemon stop inside `Checkouts::reload` at the point
where the checkouts already have their new workspaces and the crates are still the old
ones, and log `reload: paused`. The case `reload_paused` changes the manifest, has one
client ask (which sets the reload off), waits for that line, and sends 8 more clients while
the daemon is paused; it does that twice. Each answer has to be exactly the old program's
output or
exactly the new one's; anything else is WRONG. Measured: all 18 answers (9 each time, under
each backend) were the new program. They
cannot get a mix because a request is not looked at until the reload it arrived during is
done; the test now shows that instead of the argument.

### 3. The daemon widens what it builds

The two refused steps are gone. When a crate the program uses, inside the workspace, has
source that is not what rustc compiled and the daemon was not asked to build it, the daemon
adds it to what it builds and starts that program over (`Program::widen`, `Args.widened`).
The same happens for a member that first appears on a reload. `dep_not_named` now has a
crate in the middle (app uses mid uses dep) and four steps; `reload_member_added` edits the
new member. All steps match cargo.

### 4. Doctests

Implemented (`crates/blaze-engine/src/doctest.rs`, `doctest/extract.rs`, `doctest/program.rs`).
blaze stands in for rustdoc: cargo runs `$RUSTDOC --test`, and `blaze test` and
`blaze test --doc` set that to blaze. Anything that is not `--test` goes to the real
rustdoc.

- Extraction walks the crate in hir and reads each item's docs through rust-analyzer's
  `Docs` (so `#[doc = include_str!(..)]` is read as it reads it), finds code blocks with pulldown-cmark, and decides which are Rust
  with `ide_db::rust_doc::is_rust_fence`. Honoured: `no_run`, `ignore` and `ignore-<target>`,
  `should_panic`, `compile_fail` with error codes, `edition20xx`, hidden `#` lines, `##`,
  the `fn main` put around an example, the `Result` form that ends in `(())`, the injected
  `extern crate`, and crate-level attributes written in the example.
- Each example is a program of its own. rustc compiles the example's text; blaze builds the
  library once and its object is linked ahead of rustc's library in every example. So the
  example's own `main` is rustc's code and the library under it is blaze's.
- The output is libtest's: the same test names, suffixes, failure block and exit status.

Measured against `cargo test --doc`:

- the `doctest` case (21 examples, including one that fails): same under both backends;
- `harness/doctests.py` on three crates from crates.io, comparing every test line and the
  exit status: semver 1.0.28 (4 doctests), either 1.19.0 (61), smallvec 1.16.3 (14), same
  under both backends. Of the library's functions blaze compiled 43 of 107 in semver, 2 of
  172 in either and 2 of 170 in smallvec. In either the other 170 are reported as "no rustc
  symbol for function": they are generic, and their copies are made by rustc in the
  example's crate, which is rustc's code. So for either and smallvec this shows the extraction and the
  programs are right, and says little about blaze's code.

`either` found a crash in rust-analyzer's docs source map on an item whose docs have no
written line (`hir-def/src/attrs/docs.rs`, an unchecked `- 1`); fixed.

Not compared: the text of compiler errors of a `compile_fail` example, and timing lines.
When blaze cannot build the library it says `blaze: the doctests are left to rustdoc:` with
the reason and runs rustdoc.

### 5. `lto`

What it would mean: with `lto = "thin"` or `"fat"` rustc keeps bitcode in every library and
compiles the program again from that bitcode as it links, so it can inline and specialise
across crates. blaze compiles each function alone and has no step at which it sees the whole
program, so there is nothing for it to do with the setting.

Is taking it and doing nothing observably different from rustc, other than in speed and
size? For a program without undefined behaviour, no: LTO is an optimisation and keeps what
the program does. Two things do differ and neither is the program's behaviour:

- The link. rustc's LTO link line has no libraries on it, only the object it made from
  bitcode, so blaze's functions have nothing to be linked against by name. blaze therefore
  needs a link line from a build with `lto = "off"`. Given one, it accepts the profile and
  prints `lto: accepted, no cross-function optimization`. Given rustc's LTO link line it
  refuses and says so.
- `-C lto` in `RUSTFLAGS` rather than in a profile is still refused: cargo does not know
  about it and builds libraries that cannot be linked the ordinary way.

So the rows are: `lto_fat` and `lto_thin` supported (the reference is cargo with LTO, blaze
links against the same project built with `--config profile.release.lto="off"`, the outputs
are the same and the sentence is checked), and `lto_fat_linked` and `lto_thin_linked`
refused. `blaze` runs cargo itself, so with `lto` in the profile there is no ordinary
link line and the program is left to cargo with "`x` is built with LTO".

Open: when the program is a single crate there is no library of the workspace whose absence
shows the link was an LTO one, and a client that hands blaze an LTO link line gets the
linker's undefined-symbol error instead of blaze's sentence. The harness has no row for it.

### 6. One plan

There is one reader of cargo's unit graph, `crates/blaze-engine/src/plan.rs`, and one description
of which build is meant, `plan::Selection` (profile, features, `--no-default-features`,
`--all-features`, packages, `--workspace`, target, dev dependencies). The daemon's arguments
hold a `Selection`, `blaze` makes one from its command line and hands it to its worker
(`WorkerConfig.selection`), and `Selection::flags` is the only place that turns it into
cargo arguments. Rustflags come from one place too (`Flags::of`), read from the client's
environment and configuration.

Because `blaze` now uses the daemon's plan, it no longer sends these to cargo: other
profiles than dev, `--all-features`, `--no-default-features`, `RUSTFLAGS`, `opt-level`,
`panic = "abort"`, `-C target-cpu` and `-C target-feature`. `blaze_cli.py`'s workspace
was changed so these can be told apart in the output (it prints `cfg!(debug_assertions)`, a
cfg from `RUSTFLAGS`, a target feature and the result of an overflow; release is
`panic = "abort"`) and has six new commands.

What is still separate, on purpose: `cargo/units.rs` does not read the unit graph. It keeps
the records of what rustc was actually run with, written by the wrapper, and `cargo/plan.rs`
plans the link from them. Its `unsupported` still reads `-C` flags from those records to
leave a program to cargo (LTO, coverage, PGO, `-Z`).

### 7. Clippy and style

`cargo clippy -p blaze-engine -p hir-ty -p hir-def --all-targets -- --cap-lints warn` gave 305
warnings. A warning is "new" when a line it points at was written on this branch (by `git
blame` against the commits of the three rounds, or not committed yet): 12 new, 293 old. The
12 are fixed: one unused `mut`; four `Command::new` (now one function, `doctest::as_asked`,
that says why `toolchain::command` is wrong there: it adds a variable to the environment an
example can read); seven `eprintln!`, by allowing `clippy::print_stderr` for the `blaze`
binary, whose output that is. That also silences 60 or so old ones of the same kind; 234
remain, all old.

Style pass over this round's Rust: no `unwrap` or `expect` outside tests, no dead code or
`allow(dead_code)`, doc comments say what a thing is. I did not restructure code from
earlier rounds.

## Second round

### 1. A checkout is loaded again in place

Before, a change under a running daemon to `Cargo.toml`, `Cargo.lock`,
`.cargo/config.toml`, `RUSTFLAGS`, or a build-script input that changes what the script
says was answered with "start it again". Now the daemon loads that checkout's workspace
again and goes on.

How it works. `inputs.rs` keeps, for each checkout, the files cargo reads of it (manifests,
lock file, configuration files), the flag variables of the client that asked, and what the
build scripts said. Before a request is served, the checkouts it is about are asked whether
any of that is stale. For a stale one, cargo is asked again for the workspace of that
checkout only (`Checkouts::reload`), and the new crate graph is put in place of the old
with `load-cargo`'s `Loaded::replace`. Other checkouts are not looked at. Each crate is
then compared with what it was before (its data, cfg and environment): a crate that is the
same keeps its salsa state, and since the disk store is keyed on a crate's content, its
stored code is still found. Crates that differ are compiled again. When the graph itself
changed, the programs of that checkout start over: their functions are compiled again or
taken from the store, and the client's link line, sent with the request, is the new one.

A request and a reload never interleave. The daemon serves builds one at a time; a request
that arrives while a build runs waits, and staleness is checked and the reload done before
that request's build starts. So a build sees the model from before the reload or the one
after it, whole.

Two things had to be fixed on the way. Files that are new to a checkout after a reload (a
new `build.rs`, a new member) had no source root and the daemon panicked; `load-cargo` now
puts them in the root of the directory they are in. And variables that were only meant for
running cargo were also given to crates as if `env!` could read them; `project-model` now
keeps the two apart (`extra_env_for_commands_only`).

Measured. The 7 steps that were refused are "same" now (`config_env`, the `RUSTFLAGS` and
configuration rustflags cases, the manifest and build-script cases in the table). New
cases, all "same" under both backends:

- `reload_dep_added`: a dependency is added, then removed again.
- `reload_lock_version`: the version of a dependency in `Cargo.lock` goes up, then down.
- `reload_feature_toggled`: a feature of a dependency and a default feature of the package
  are turned on in the manifest, then both off, with 6
  clients asking at once after each change.
- `reload_member_added`: a new workspace member is added and depended on.
- `reload_build_script_added`: a `build.rs` appears that sets a cfg and an env variable,
  then says something else.
- `reload_git_checkout`: `git checkout` between two commits that differ in all of the
  above, there, back and there again, with 6 clients asking at once each time.

`worktrees.py` has a new step where the manifest of one of several checkouts changes in a
way no crate reads; it passes 62 of 62 (plain, `--join`, `--together`) and 71 of 71
(`--overlay`), which includes its counts of what each checkout compiles.

Inferred, not measured: that "6 clients at once" exercises every interleaving. It shows
that six requests arriving together after a change all get the new program; the argument
that none can see a mix is the one above, from how the daemon orders builds, not a test
that tries to catch one mid-reload. How long a reload takes was not measured either; it is
one `cargo metadata` and the build scripts of that checkout.

One thing this turned up, which was silently wrong before and is a refusal now. A daemon
asked to build only `app` (`--crate app`) took an edit to a path dependency of `app`,
answered "ok", and the program still ran rustc's old copy of the dependency. That is now
refused: "... is of `dep`, which the program uses and the daemon was not asked to build:
rustc's copy of it is from before the edit, name the crate with `--crate`". Cases
`dep_not_named` and the second step of `reload_member_added` (an edit inside a member that
did not exist when the daemon was told what to build). After a reload the refusal is not
given, because the client has just sent a link line from a fresh cargo build.

### 2. Debug info from the unit graph

`crates/blaze-engine/src/profile.rs`, which read `Cargo.toml` itself, is deleted, and the `toml`
dependency of `blaze` with it. `Plan::debug()` in `plan.rs` says whether the packages the
build was asked for have debug info, from the profile of their units in cargo's unit graph,
with `-g` and `-C debuginfo=` of the rustflags over it. That sees `.cargo/config.toml`
profiles and per-package overrides because cargo has already applied them. It is kept per
checkout and read again on a reload. `BLAZE_DEBUGINFO` still overrides it. `debuginfo.py`
passes under both backends.

### 3. Overflow checks and debug assertions per package

Both rows that were refused (`overflow_checks_package`, `release_package_checks`) are
supported.

`debug-assertions` was already per crate, since it is a cfg. For overflow checks, an
instance of a function now carries whether it is compiled with checks (`Instance::checked`
in `codegen.rs`). A function that is neither generic nor `#[inline]` takes the setting of
the crate that defines it. A generic or `#[inline]` function takes the setting of the code
that asks for it, so the same generic function can exist twice in one program, once
checked and once not. The stable name of a checked instance ends in " checked", and that
name is what the store is keyed on, so the two never share an entry; the single
whole-build overflow setting is gone from `Entry::key`.

`keys.py` has 10 more cases (46 of 46 right, 0 open, both backends): overflow checks
changed for a dependency only, for the program crate only, for a generic function used
from two crates with different settings, the same in release, and debug assertions for one
package. Each builds with an empty store, changes the setting, builds with the warm store
in place and in a copy elsewhere, and compares with cargo.

Known differences at the end of this round (the first two are closed in the third round):

- With `-Z share-generics`, which is on by default at `opt-level` 0, rustc may reuse an
  instantiation that an upstream crate already made, with the upstream crate's setting.
  blaze always uses the setting of the crate that asks. The fixtures do not show a
  difference; a program built so that it depends on which copy rustc reuses could.
- In release rustc can decide by itself that a small non-generic function is inlinable
  across crates and compile it in the caller, with the caller's setting. blaze goes by
  `#[inline]` only. The fixture's function for "defined in the dependency" is
  `#[inline(never)]` for that reason.
- The `ub_checks` intrinsic is a constant false, as before.

### 4. Doctests

Refused in this round; implemented in the third, see above.

### 5. `[target.'cfg(..)'] rustflags`

Supported (`config_rustflags_cfg`). The keys of `[target]` tables are matched with
`cargo-platform` against the triple and the cfg of the target as rustc prints it, and the
flags of every table that matches are joined the way cargo does. The refusal is gone.

### `lto`

Refused in this round; see the third round for what is accepted now.

## First round: what was wrong and what was done

The first run of the suite, before any fix, had 19 WRONG rows of 87 under Cranelift.

Fixed at the root:

- Profiles. blaze had no notion of one and compiled everything as dev. It now takes
  `--release` and `--profile`, asks cargo for the unit graph of the build (`plan.rs`), and
  gives each package the `debug_assertions`, overflow checks and panic strategy cargo builds
  it with. Those are part of the crate's cfg, so of its hash in `content.rs`, and the
  overflow setting is in `cached.rs` `Entry::key`, so a dev entry is never served to a
  release build. `harness/keys.py` "the release profile" is closed and has three more
  cases (overflow checks, debug assertions, a custom profile).
- `env!` and `option_env!` of a variable of the process. They only saw what cargo and build
  scripts set. The client now sends its environment with each request, the macros read the
  crate's own variables and then those of the client, and the values a crate read are part
  of its identity. A variable nothing reads does not invalidate anything; two clients with
  different values do not share code. `keys.py` "a variable of the process read by
  option_env!" is closed.
- `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, and `[build] rustflags` / `[target.<triple>]
  rustflags` of `.cargo/config.toml`. `-C debug-assertions`, `-C overflow-checks`,
  `-C panic` and `-C opt-level` come after the profile's flags on rustc's command line and
  win; blaze ignored them. They are now read the way cargo orders them and applied over the
  profile of every package.
- `--features`, `--no-default-features`, `--all-features`, `-p` and `--workspace`. The
  features of a package are now those of the unit cargo builds, not those of `cargo
  metadata`, which unifies over the whole workspace. That is what makes resolver 1, 2 and 3
  and "one member built" against "the workspace built" come out as cargo has them.
- `CARGO_PRIMARY_PACKAGE` was never set. It is set for the packages the build was asked for.
- `CARGO_PKG_RUST_VERSION` was `1.70.0` where the manifest says `1.70`. It is now what the
  manifest has, as cargo passes it.
- `--target aarch64-unknown-linux-gnu`: `std::env::consts::ARCH` printed
  `UNRESOLVED_ENV_VAR`, because the build script of `std`, which sets `STD_ENV_ARCH`, is
  never run. The variable is now set on the sysroot's `std` from the target's cfg.
- `#![no_main]`: blaze wrote a `main` that calls `lang_start` for any crate with a function
  named `main` at its root, which is wrong for a crate that exports its own. A crate with
  `#![no_main]` gets none.
- `#![no_std]` binary that defines `rust_eh_personality` itself (the usual way on stable):
  the Cranelift backend declared the runtime's personality up front, with a signature the
  program's own function did not have, and failed. The personality is now named once every
  function is declared, and the program's own is used when it has one.
- Files a build script writes into `OUT_DIR` were taken for files that never change, so
  generated code was stale after the script ran again. They are watched now.

Turned into a refusal in the first round: `lto`; and four that the second round has since
dealt with (per-package overflow checks, `[target.'cfg(..)'] rustflags`, changes that
needed a restart, doctests).

Two rows are refusals by cargo itself (`env!` of an unset variable, a `rust-version` newer
than the toolchain); the case checks that blaze turns the project down too.

## What is measured and what is inferred

Measured: the table below; the regression gate; `keys.py`.

Inferred, not measured:

- "supported" for `opt-level`, `codegen-units` and `strip` means the program behaves the
  same. blaze's own optimisation level is its own (`BLAZE_OPT`), not the profile's, and the
  suite does not compare speed or size.
- `-C target-cpu` and `-C target-feature` are supported in that `cfg(target_feature)` and
  the program's output agree with rustc. Whether blaze's backends emit the wider
  instructions was not looked at.
- The cross rows run under qemu (aarch64) and natively (musl). Other targets were not run.
- The points marked as inferred under "A checkout is loaded again in place" and the known
  differences under "Overflow checks and debug assertions per package".

## Still open

- Overflow checks in a program whose crates differ in the setting: an edit under the
  daemon is refused until cargo has built again, where a narrower rule (only edits that
  move a copy) would let most edits through. Generic code that std ships a copy of was not
  examined.
- `lto`: a client that links by itself with rustc's LTO link line gets the linker's error;
  blaze only sees a link command it is given. `-C lto` in `RUSTFLAGS` is refused.
- Doctests: rustdoc prints nothing of the errors of a `compile_fail` example that fails to
  compile as it should, so those are not compared; where blaze cannot build the library the
  examples are left to rustdoc with the reason printed (none of the 30 crates).
- `cargo/units.rs` still keeps its own records of what rustc was run with (it does not read
  the unit graph), and `cargo/plan.rs` plans the link from them.
- `Flags::of` in `plan.rs` asks `cargo config get` for the configuration, which is how
  project-model already reads it; it is an unstable cargo command.

## Where this stands

Done and measured under both backends: everything in this report up to and including the
merge of 14ee89181b, with the whole gate in the table below.

Half done: the merges after that. `blaze-collate` 3d47ed59bf is merged (094c139694, one
conflict in the flag cases of `harness/blaze_cli.py`; both sides' cases are kept, and the
case with `-C target-cpu=x86-64-v3` only runs on an x86-64 host). The whole Cranelift leg
passed on that merge; its LLVM leg was stopped after `buildmodel.py` (105 of 105) to merge
the next head. `blaze-collate-next` 1b022ff6f2 is merged on top (03e4d294df, conflicts in
`codegen.rs`, `codegen/cached.rs`, `cargo/plan.rs`, `main.rs`, `outline.rs`, `symbols.rs`).
What was kept: both sides' additions to `SymbolResolver`; the timed store read with this
branch's program identity in the key; `LinkLine::command` with this branch's signature (the
objects ahead, the binary) over the new MSVC flavour; `crates_to_build` from the other side,
asked for every crate of `Args::crates()`; `Listed::left_to_rustc` stays removed, since the
daemon widens what it builds instead. A unit test of `cargo/plan.rs` was brought to the
merged signature (f030070c92). On this head: it builds with `--features llvm`, `cargo fmt`
is clean, the unit tests pass (1026, 0 failed), and the Cranelift leg of the gate was run,
see "The gate on the head" below. The LLVM leg was not run on this head, because work was
stopped; the MSVC link path of `LinkLine::command` is only covered by its unit test here.

What would come next, in order: the LLVM leg of the gate on the head; then the open items
above (generic copies of the standard library in programs that differ in
`overflow-checks`; `cargo/units.rs` reading the planner's unit graph; `Flags::of` without
the unstable `cargo config get`; `blaze` with `lto` in the profile; diagnostic text
of `compile_fail` doctests).

The fix of the build-script markers of `selects.py` (one write for a line) is in, since
2c2ba1432d.

### The gate on the head

Run with `BLAZE_BACKEND=cranelift` only, on the binary of 03e4d294df (the head differs from
it by the unit test alone). Every harness of the gate passed:

| command | result |
|---|---|
| `python3 buildmodel.py --backend cranelift --jobs 6` | 105 of 105 as expected, 0 WRONG |
| `python3 keys.py` | 52 of 52 right, 0 open, 8 functions refused |
| `python3 difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ |
| `python3 programs.py` | 22 of 22 right |
| `python3 stress.py` | 99 of 99 right, 0 restarts |
| `python3 breakers.py` | 94 of 94 right, 6 open, 0 functions skipped |
| `python3 unwinds.py` | 8 of 8 right |
| `python3 splices.py` | 71 of 71 right |
| `python3 asmedits.py` | 10 of 10 right |
| `python3 includes.py` | 9 of 9 right |
| `python3 worktrees.py` | 62 of 62 right |
| `python3 debuginfo.py` | all right |
| `python3 storecorrupts.py` | PASS, 40 damaged stores |
| `python3 storefull.py` | PASS, 6 full disks |
| `python3 storekills.py` | PASS, 300 kills |
| `python3 storelib.py` | exit 0 |
| `python3 storeshared.py` | PASS, 180 builds |
| `python3 storeguards.py` | all right |
| `python3 selects.py` | 3 of 3 right |
| `python3 remote.py` | 117 of 117 right |
| `python3 fuzz/fuzz.py smoke --backends cranelift` | exit 0 |
| `python3 blaze_cli.py --only-checks` | PASS |
| `python3 doctests.py` | 30 same, 0 refused, 0 WRONG |

## Regression gate

The table of this section is of the merge of 14ee89181b, the last head both legs ran on.


Run from `harness/` with `BLAZE_BIN` set to the release binary built with `--features llvm`
from the head of the branch (the merge of `blaze-collate` 14ee89181b and the fix of blaze in
rustdoc's place after it), once with `BLAZE_BACKEND=cranelift` and once with
`BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`, one after the other. The numbers were the
same under both.

The Cranelift leg ran on the binary of the merge itself, where `blaze_cli.py` and
`doctests.py` failed on blaze taking itself for the rustc wrapper. Those two were run again
under Cranelift on the fixed binary and pass; the other Cranelift rows are of the binary
before the fix, which changes only which part of blaze answers when cargo runs it as
rustdoc. The whole LLVM leg ran on the fixed binary.

| command | result |
|---|---|
| `python3 difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 822/822 functions from blaze, 0 of 893 layouts differ |
| `python3 backends.py corpus runner cases --bin runner --levels 2f` (LLVM leg) | the program prints the same as cargo's; LLVM produced 11801 of 11801 functions (100%) |
| `python3 programs.py` | 22 of 22 right |
| `python3 stress.py` | 99 of 99 right, 0 restarts |
| `python3 breakers.py` | 62 of 62 right, 7 open, 0 functions skipped |
| `python3 unwinds.py` | 8 of 8 right |
| `python3 splices.py` | 71 of 71 right |
| `python3 asmedits.py` | 10 of 10 right |
| `python3 includes.py` | 9 of 9 right |
| `python3 worktrees.py` | 62 of 62 right |
| `python3 keys.py` | 52 of 52 right, 0 open, 8 functions refused |
| `python3 debuginfo.py` | all right |
| `python3 buildmodel.py --backend <backend> --jobs 6` | 105 of 105 as expected, 0 WRONG |
| `python3 doctests.py` | 30 crates the same as `cargo test --doc`, 0 left to rustdoc, 0 WRONG |
| `python3 storecorrupts.py` | PASS, 40 damaged stores detected, discarded and rebuilt |
| `python3 storefull.py` | PASS, 6 full disks |
| `python3 storekills.py` | PASS, 300 kills |
| `python3 storeshared.py` | PASS, 180 builds on a shared store |
| `python3 storeguards.py` | all right |
| `python3 selects.py` | 3 of 3 right |
| `python3 remote.py` (MinIO in `~/s3tools/minio`) | 117 of 117 right |
| `python3 fuzz/fuzz.py smoke --backends <backend>` | 2 programs, 505 functions compiled by blaze, 0 differences, 0 crashes |
| `python3 blaze_cli.py --only-checks` | PASS |
| `cargo fmt --all -- --check` | clean |
| `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p blaze-engine` | 1021 passed, 0 failed |

The open cases of `breakers.py` came with `blaze-collate` and are not of this work.
`blaze_cli.py` was run without its timings of real projects. Clippy on `blaze` has no
warning in the lines this round touched.

## The table

One row per case. "invalidation" is what each step of the case came to under one daemon.

| feature | case | backend | result | detail | invalidation |
|---|---|---|---|---|---|
| benches (`harness = false`, the `bench` profile) | `bench` | both | supported |  |  |
| `codegen-units = 1` | `codegen_units` | both | supported |  |  |
| `codegen-units = 1` in dev | `codegen_units_dev` | both | supported |  |  |
| `.cargo/config.toml`: `[env]` | `config_env` | both | supported |  | the value in the configuration changes: same |
| `.cargo/config.toml`: `[env]` under a variable of the process, and `force` | `config_env_overridden` | both | supported |  |  |
| `.cargo/config.toml`: `[target.*] linker` and `runner` | `config_linker_runner` | both | supported |  |  |
| `.cargo/config.toml`: `[build] rustflags` | `config_rustflags` | both | supported |  |  |
| `.cargo/config.toml` `[target.'cfg(..)'] rustflags` | `config_rustflags_cfg` | both | supported |  |  |
| `.cargo/config.toml` `[target.<triple>] rustflags` | `config_rustflags_target` | both | supported |  |  |
| crate types `bin` and `lib` | `crate_bin_lib` | both | supported |  |  |
| crate type `cdylib` | `crate_cdylib` | both | supported |  |  |
| crate type `dylib` | `crate_dylib` | both | supported |  |  |
| crate types `dylib` and `rlib` together | `crate_dylib_rlib` | both | supported |  |  |
| crate type `proc-macro` (rustc's build of it is loaded; its expansion is compiled by blaze) | `crate_proc_macro` | both | supported |  |  |
| crate type `rlib` | `crate_rlib` | both | supported |  |  |
| crate type `staticlib` | `crate_staticlib` | both | supported |  |  |
| `--target aarch64-unknown-linux-gnu` | `cross_aarch64` | both | supported |  |  |
| `--target x86_64-unknown-linux-musl` | `cross_musl` | both | supported |  |  |
| `debug-assertions = false` in dev | `debug_assertions_off` | both | supported |  |  |
| `debug-assertions = true` in release | `debug_assertions_on` | both | supported |  |  |
| per-package `debug-assertions` | `debug_assertions_package` | both | supported |  |  |
| git dependency | `dep_git` | both | supported |  |  |
| edit to a dependency the daemon was not asked to build | `dep_not_named` | both | supported |  | the dependency's source changes: same; a generic function of it that a crate between holds a copy of changes: same; the crate between changes: same; the dependency changes back: same |
| `[patch]` | `dep_patch` | both | supported |  | the patched source changes: same |
| path dependency outside the workspace | `dep_path` | both | supported |  | the dependency's source changes: same |
| a workspace member that is also a registry dependency at the same version | `dep_registry_member` | both | supported |  |  |
| renamed dependency (`package = ...`) | `dep_renamed` | both | supported |  |  |
| `[replace]` | `dep_replace` | both | supported |  |  |
| vendored dependency (`[source] directory`) | `dep_vendored` | both | supported |  |  |
| doctests | `doctest` | both | supported |  |  |
| doctests with `--target aarch64-unknown-linux-gnu` | `doctest_cross` | both | supported |  |  |
| doctests of two members of a workspace, one using the other | `doctest_members` | both | supported |  |  |
| editions 2015, 2018, 2021 and 2024 in one graph | `editions_mixed` | both | supported |  |  |
| `CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`, `CARGO_CRATE_NAME`, `CARGO_BIN_NAME` | `env_cargo_pkg` | both | supported |  | the version in the manifest changes: same |
| `env!` of a variable of the process | `env_env` | both | supported |  | the variable changes: same |
| `env!` of a variable that is not set | `env_env_unset` | both | refused | cargo turns it down too |  |
| `option_env!` of a variable of the process | `env_option_env` | both | supported |  | the variable changes: same; another variable, which nothing reads, changes: same |
| examples | `example` | both | supported |  |  |
| `--all-features` | `features_all` | both | supported |  |  |
| `[features]`: defaults, `default-features = false` on a dependency | `features_default` | both | supported |  |  |
| `--no-default-features` | `features_none` | both | supported |  |  |
| optional dependency through `dep:` | `features_optional_dep` | both | supported |  |  |
| `--features` enabling a dependency's feature (`lib/fancy`) | `features_selected` | both | supported |  |  |
| weak `opt?/loud` with the optional dependency off | `features_weak_off` | both | supported |  |  |
| weak `opt?/loud` with the optional dependency on | `features_weak_on` | both | supported |  |  |
| integration tests | `integration_test` | both | supported |  |  |
| `links` and `DEP_<name>_<key>` metadata | `links` | both | supported |  |  |
| `lto = true` | `lto_fat` | both | supported |  |  |
| `lto = true`, and what blaze links against built with it | `lto_fat_linked` | both | refused | rustc linked the program without a library of `dep`, as it does with `lto`: blaze links against a build with `lto = "off"` |  |
| `lto = "off"` | `lto_off` | both | supported |  |  |
| `lto = true` in a program of one crate, blaze given the link line | `lto_single` | both | supported |  |  |
| `lto = true` in a program of one crate, blaze given the link line built with it | `lto_single_linked` | both | refused | the link command has no library of `core`, as rustc's has not with `lto`: blaze links against a build with `lto = "off"` |  |
| `lto = "thin"` | `lto_thin` | both | supported |  |  |
| `lto = "thin"`, and what blaze links against built with it | `lto_thin_linked` | both | refused | rustc linked the program without a library of `dep`, as it does with `lto`: blaze links against a build with `lto = "off"` |  |
| proc macro: attribute | `macro_attribute` | both | supported |  |  |
| proc macro: derive, with a dependency of its own | `macro_derive` | both | supported |  |  |
| proc macro: function-like | `macro_function` | both | supported |  |  |
| multiple `[[bin]]` targets | `multiple_bins` | both | supported |  |  |
| `#![no_main]` with std | `no_main` | both | supported |  |  |
| `#![no_std]` binary with `#![no_main]` | `no_std_no_main` | both | supported |  |  |
| per-dependency `opt-level` (`[profile.dev.package."*"]`) | `opt_level_dependencies` | both | supported |  |  |
| `opt-level = 3` in dev | `opt_level_dev` | both | supported |  |  |
| per-package `opt-level` (`[profile.dev.package.dep]`) | `opt_level_package` | both | supported |  |  |
| `overflow-checks = false` in dev | `overflow_checks_off` | both | supported |  |  |
| `overflow-checks = true` in release | `overflow_checks_on` | both | supported |  |  |
| per-package `overflow-checks` | `overflow_checks_package` | both | supported |  |  |
| `overflow-checks` of one package, and an edit that moves a function to where rustc compiles it in its callers | `overflow_checks_stale` | both | supported |  | the function becomes small, asked with what cargo linked before: refused (the crates of the program differ in `overflow-checks` and dep/src/lib.rs was written after rustc built it: which crate's copy of a function a call reaches is read from what rustc built, so cargo has to build first); cargo builds the small function, and the client asks with what it linked: same; the function becomes large again, asked with what cargo linked before: refused (the crates of the program differ in `overflow-checks` and dep/src/lib.rs was written after rustc built it: which crate's copy of a function a call reaches is read from what rustc built, so cargo has to build first); cargo builds the large function, and the client asks with what it linked: same |
| `panic = "abort"` | `panic_abort` | both | supported |  |  |
| `panic = "abort"` in release | `panic_abort_release` | both | supported |  |  |
| `panic = "unwind"` (the default) | `panic_unwind` | both | supported |  |  |
| profile: custom (`--profile fast`, inherits release) | `profile_custom` | both | supported |  |  |
| profile: dev | `profile_dev` | both | supported |  |  |
| profile: release | `profile_release` | both | supported |  |  |
| per-package checks turned on in release | `release_package_checks` | both | supported |  |  |
| reload: a new `build.rs` | `reload_build_script_added` | both | supported |  | the package gets a build script: same; the script says something else: same |
| reload: a dependency is added to the manifest and removed again | `reload_dep_added` | both | supported |  | a dependency is added: same; and removed: same |
| reload: a feature is toggled in the manifest | `reload_feature_toggled` | both | supported |  | a feature of a dependency is turned on: same; a default feature of the package is turned on: same; and both off: same |
| reload: `git checkout` between commits that differ in dependencies, lock file, features, members and build script | `reload_git_checkout` | both | supported |  | the other commit is checked out: same; and the first again: same; and the other once more: same |
| reload: a dependency's version is bumped in `Cargo.lock` | `reload_lock_version` | both | supported |  | the lock file has the newer version: same; and the older again: same |
| reload: a new workspace member | `reload_member_added` | both | supported |  | a member is added and depended on: same; the new member's source changes: same |
| reload: requests that arrive in the middle of one | `reload_paused` | both | supported |  | 8 clients ask while two features are turned on: same; 8 clients ask while one of them is turned off: same |
| `rust-version` the toolchain meets | `rust_version_met` | both | supported |  |  |
| `rust-version` newer than the toolchain | `rust_version_unmet` | both | refused | cargo turns it down too |  |
| `RUSTFLAGS` with `--cfg` | `rustflags_cfg` | both | supported |  |  |
| `RUSTFLAGS=-C debug-assertions=off` over the profile | `rustflags_debug_assertions` | both | supported |  | `RUSTFLAGS` of the client is another: same |
| `RUSTFLAGS=-C opt-level=3` in dev: the checks cargo left to rustc go off | `rustflags_opt_level` | both | supported |  |  |
| `RUSTFLAGS=-C overflow-checks=off` over the profile | `rustflags_overflow_checks` | both | supported |  |  |
| `RUSTFLAGS=-C panic=abort` over the profile | `rustflags_panic_abort` | both | supported |  |  |
| `RUSTFLAGS=-C target-cpu=x86-64-v3` | `rustflags_target_cpu` | both | supported |  |  |
| `RUSTFLAGS=-C target-feature=+avx2,+fma` | `rustflags_target_feature` | both | supported |  |  |
| build script: `cargo:rustc-link-lib` and `rustc-link-search` | `script_link_lib` | both | supported |  |  |
| build script: generated code through `OUT_DIR` | `script_out_dir` | both | supported |  | `rerun-if-changed` input gains a row: same |
| build script: `cargo:rerun-if-changed` | `script_rerun_if_changed` | both | supported |  | the named file changes: same; the build script itself changes: same |
| build script: `cargo:rerun-if-env-changed` | `script_rerun_if_env_changed` | both | supported |  | the variable is set: same; the variable changes: same |
| build script: `cargo:rustc-cfg` | `script_rustc_cfg` | both | supported |  | the file the script reads turns the cfg off: same; and on again: same |
| build script: `cargo:rustc-env` | `script_rustc_env` | both | supported |  | the file the script reads has another value: same |
| `strip = "debuginfo"` with `debug = true` | `strip_debuginfo` | both | supported |  |  |
| `strip = true` | `strip_symbols` | both | supported |  |  |
| target-specific dependencies and `cfg(target_*)` on the host | `target_specific_deps` | both | supported |  |  |
| feature unification across members, one package built (`-p app`) | `unify_members` | both | supported |  |  |
| feature unification, resolver 1 (host and target unify) | `unify_resolver1` | both | supported |  |  |
| feature unification, resolver 2 (host apart from target) | `unify_resolver2` | both | supported |  |  |
| feature unification, resolver 3 (edition 2024) | `unify_resolver3` | both | supported |  |  |
| feature unification across members, whole workspace built | `unify_workspace` | both | supported |  |  |
| unit tests | `unit_test` | both | supported |  |  |
