---
status: plan
summary: Lane pure-scripts: a build script whose MIR proves it only reads cargo's variables and its package's files, writes its OUT_DIR and prints, is keyed by its MIR identity and those inputs and, on a hit, is neither compiled nor run.
date: 2026-10-30
---
# Pure build scripts

Approved by Andrew (2026-10-30): keep the sandbox (security, exact input tracking) and add a
fast path. `crates/blaze-build-script/src/elide.rs` already reads scripts that only print
constants. This lane proves more scripts pure from their MIR, at the pinned nightly's semantics,
in blaze's own front end.

## 1. What pure means

A script is **pure** when its whole reachable call graph (its own code, its build dependencies'
and std's, monomorphized from `main`) does only this:

- (a) reads environment variables cargo gives a build script, named by a constant: `CARGO_*`
  but `CARGO_MAKEFLAGS` (the jobserver: of one run alone), `TARGET`, `HOST`, `PROFILE`,
  `OPT_LEVEL`, `DEBUG`, `OUT_DIR`, `NUM_JOBS` ([`hir_ty_pure::allowed_env`]). `RUSTC`,
  `RUSTDOC`, `RUSTC_WRAPPER`, `RUSTC_LINKER`, `DEP_*` and anything passed through are refused;
- (b) reads files inside its package and writes files inside `OUT_DIR`, at paths made only from
  `CARGO_MANIFEST_DIR`, `OUT_DIR` and constant relative paths with no `..` (a relative path is
  inside the package: the script runs in its manifest directory); tests whether such a package
  path exists (`Path::exists`, `Path::is_file`);
- (c) prints to stdout (and stderr, which is part of the kept result and as deterministic);
  exits (`process::exit`) or panics;

and nothing else: no process, no network, no other file, no listing, no metadata, no clock, no
randomness (std's `RandomState` reaches the OS and is refused), no thread, no FFI but the
allocator's functions, no inline assembly, no raw syscall, no state in a `static mut`, a
thread-local or a foreign static, no address made from a number, no call through a function
pointer or `dyn` whose targets the graph does not close (section 3).

Anything the analysis can't prove is **not pure**: the script compiles and runs in the sandbox
as today. That is the design, not a fallback: the sandboxed run is the general path, the pure
path an extra key for scripts that qualify.

## 2. Keys: a constructive trace over the MIR identity

A pure script's output is a function of its code and the inputs (a) and (b). It is kept as
another run of `blaze_usercode`'s index (`crates/blaze-usercode/src/index.rs`), under

- `RunKey { code: proof.identity, given: H("blaze v2 pure build script 1") }`: the identity is
  the Merkle hash of the instance graph (section 4); nothing else is given (no arguments, empty
  stdin, the working directory is the package, whose files are in the trace by absolute path);
- a shape of: the variables of `proof.env` (the names the code can read, whether or not this run
  read them), and the files the sandboxed run read inside its package, by path, with no
  listing and nothing untraceable.

The result key is the shape's values now: each variable's value in the run's environment and
each file's content hash. This is Shake's constructive trace: a run is deterministic, so if
every value it read is what it was, it reads the same and prints the same. On a hit
(`blaze_usercode::pure::find`) the script is neither compiled nor run: its `OUT_DIR` is written
back from the kept value as any kept run's is. The record goes into the same store and the same
global tier as every run (`crates/blaze-cargo/src/serve/unit/script/global.rs`).

The variables' values come from the unit alone (none of (a) is a `DEP_*` or passed through),
so a hit is known before the script's build dependencies' scripts ran.

What the key does not do: relocate paths. `OUT_DIR` and `CARGO_MANIFEST_DIR` are absolute, so
a script that reads them is shared between machines with the same layout (the ix VMs, CI), not
across layouts. Relocating would need a proof that the values flow only into path roots and
whole printed paths; it is left out on purpose, not for later.

## 3. Closed world

- Every function turned into a pointer (`ReifyFnPointer`, `ClosureFnPointer`) anywhere in the
  graph is walked as if called. Every unsizing to `dyn Trait` of a concrete type walks the
  methods of `Trait` (and its supertraits, and drop) for that type.
- A call through a function pointer or `dyn` is then closed: its target is one of those, all
  walked. It is open, and the script not pure, if the graph has a source of function pointers
  it can't see: an address made from a number (`PointerWithExposedProvenance`), a `transmute`
  to a type holding a function pointer or a `dyn` pointer, a constant or static whose value
  holds a function pointer or a vtable the walk did not see made, or a foreign function.
- Drops run drop glue: each `Drop` impl of the type and its fields, walked.

## 4. The identity

The instances are walked depth first from `main` in call-site order (block order, statement
order), each once. The identity is BLAKE3 over, for each instance in that order, the item
identity of its body (`blaze_item_hash::identity::identity`; a closure's is its parent's with
the closure's index in it; a std leaf of section 5 is its item key and its model's tag) and the
indices of its callees in that order. A change to any reached body, to which impl a call
resolves to, or to the toolchain, target, cfg or features (all in every identity's crate inputs)
changes it.

## 5. std at its edge: leaves and transfers

std is walked like any crate, so whatever is not modeled reaches its FFI and is refused. A short
table (`crates/hir-ty-pure/src/models.rs`) names, by path in std, the functions where the walk
stops (**leaves**, with what each does) and the functions whose bodies are walked but whose
values the path analysis follows (**transfers**):

| model | functions | meaning |
|---|---|---|
| leaf `EnvVar` | `env::var`, `env::var_os` | reads the variable named by the constant argument; allowed only if [`allowed_env`] |
| leaf `Print` | `io::_print`, `io::_eprint`, `io::stdout`, `io::stderr`, their locks, `Write` for them | prints |
| leaf `ReadFile` | `fs::read`, `fs::read_to_string`, `File::open` | reads a package path |
| leaf `WriteFile` | `fs::write`, `File::create`, `fs::create_dir`, `fs::create_dir_all` | writes an `OUT_DIR` path |
| leaf `Copy` | `fs::copy` | reads a package path, writes an `OUT_DIR` path |
| leaf `Probe` | `Path::exists`, `Path::is_file` | tests a package path |
| leaf `Handle` | `Read`/`Write`/`Seek` for `File` and `&File`, the descriptor's close | through a handle the leaves above opened |
| leaf `Exit` | `process::exit`, `process::abort` | ends the run |
| leaf `Panic` | the panic entry points (`core::panicking::*`, `std::panicking::begin_panic*`, `rust_panic_with_hook`, `rust_begin_unwind`, `Result`'s and `Option`'s `unwrap_failed`/`expect_failed`) | fails the run: never kept, so nothing on the way to a panic (an error's `Debug` that `unwrap` formats) can reach a kept value |
| foreign leaf | `strerror_r`, `__xpg_strerror_r` (unix) | an error's text: Rust never calls `setlocale`, so it is the C locale's, fixed; `FormatMessageW` (windows) depends on the user's language and is refused |
| leaf `Alloc` | the allocator's functions (`#[rustc_allocator]` family, `__rust_*`) | new memory |
| transfer | `Path::new`, `PathBuf::from`, `From`/`AsRef`/`Deref`/`Borrow` between `str`, `String`, `OsStr`, `OsString`, `Path`, `PathBuf`, `Path::join`, `PathBuf::push`, `to_path_buf`, `to_owned`, `clone`, `Result`/`Option` `unwrap`, `expect`, `?` (`Try::branch`) | the path or string value follows |

## 6. The path analysis

Per instance, flow-insensitively, each local has an abstract value ([`hir_ty_pure::Abs`]):

- `Const(s)`: a constant string;
- `Root(Package | OutDir)`: `CARGO_MANIFEST_DIR`'s or `OUT_DIR`'s value, or it joined with safe
  relative constants (no `..`, not absolute, no prefix or root); a safe relative constant used as
  a path is `Root(Package)`;
- `Param(i)`: the instance's parameter `i`, as it was given (in summaries only);
- `Unknown`.

Assignments join (two different values are `Unknown`). A projection of a local is the local's
value (a `Result<String, _>`'s payload is the string's). A reference is its referent's value. A
mutable borrow passed to anything but a transfer that names it makes the local `Unknown`; an
assignment through a dereference makes every local whose address was taken `Unknown`.

Summaries are per instance: its effects, the variables it reads, the leaf effects whose
argument is `Param(i)` (substituted at each call site with the caller's value), the value it
returns (a concrete value or `Param(i)`), and the first impurity with the calls leading to it.
Cycles of calls start empty and iterate to their fixpoint; the effects only grow.

## 7. The proof is checked by the first run

On a miss the script compiles and runs in the sandbox, traced, as today. When it ends
(`blaze_usercode::pure::check`), what the trace shows is checked against the proof, and any
effect the proof said absent is an error naming the script and the effect, a bug in the
analysis, never a silent reclassification:

- a file read outside the package and `OUT_DIR` (but the program itself and what the loader and
  the runtime read before `main`: the system's library directories and loader files, the
  toolchain's directories), or a package read when the proof says it reads none;
- a listing, anything untraceable (clock, randomness, network, a descriptor), a refused write or
  address, an incomplete record.

The environment is not checked against the trace: the launcher records the whole environment
for every run (`docs/v2/plan/lanes/usercode/notes.md`, "The environment"). Only a run that
succeeded and passed the check is kept under the pure key; it is kept under its own run key too,
as today.

`BLAZE_PURE=verify` runs a pure script that hit as well and fails when its value differs from
the kept one: the differential the harness runs.

## 8. Measurement and the differential

`harness/v2/pure_scripts.py` (by request to lane proof) builds the harness apps and a corpus
of the most downloaded crates.io packages with build scripts, and reports per script: pure or
the first impurity (the build report's purity of each script run, from
`script::pure::reported`), and the cold-build time saved; with `BLAZE_PURE=verify` every pure
hit's value must equal the sandboxed run's. A script that uses `cc` is never pure (it spawns
the compiler) and the harness asserts that.

No hook makes a store that holds only the pure keys. Three passes share one store instead.
Pass 1 is cold with an empty store: everything is sandboxed and the pure keys are kept. Pass 2
adds `RUSTFLAGS='--cfg <fresh>'`. That changes every compile key but no pure key, because the
MIR identity is the call graph's and the key's variables are the proof's, so every unit builds
cold and the pure scripts are neither built nor run. A script whose MIR uses that cfg, or that
reads `CARGO_ENCODED_RUSTFLAGS`, misses, and the harness reports it. Pass 3 uses another fresh
cfg with `BLAZE_PURE=verify`: everything is built and run, and every pure value is checked.
That gives both the differential and the control time. The time saved is pass 3 minus pass 2.

## 9. Paths

| path | owner |
|---|---|
| `crates/hir-ty-pure/` (new front-end crate: the walk, the path analysis, the models, the identity, tests) | pure-scripts |
| `crates/blaze-usercode/src/pure.rs`, `pure/` (new files in usercode's crate: key, shape, check, find, keep) | pure-scripts |
| `crates/blaze-cargo/src/serve/unit/script/pure.rs` (new file in cargo's crate: the glue) | pure-scripts |
| hooks in `crates/blaze-cargo/src/serve/unit/script.rs`, `compile.rs`, `unit.rs` | cargo, by request |
| `harness/v2/pure_scripts.py` | pure-scripts, by request to proof |
| `docs/v2/plan/lanes/pure-scripts/` | pure-scripts |
