# What goes in the rust-analyzer subtree, and what stays in crates/

**Superseded in part, 2026-10-28** (Andrew: "We don't care about upstreaming rust-analyzer
anymore"): `vendor/rust-analyzer` is no longer a synced fork. Its crates are in `crates/`, members
of the root workspace with one `Cargo.lock`, the fork repositories are archived, `vendor/subtree.py` and CI's
`fork alone` job are gone, and steps 5 and 6 below are withdrawn. Where new code goes (sections 1
to 3) still holds.

Date: 2026-10-21. A design and a plan, being carried out: steps 0 (the rebuild times, 3.3), 2
(the code generator a crate), 3 (the store a crate), 4 (the borrow checker into the subtree)
and 5 (the fork builds alone, checked in CI) are done, see their
rows in section 4. The rest of this text describes the tree before them. Read against blaze `main` at
`22b1267233`, the fork `indexable-inc/rust-analyzer` `blaze` at `c0dabc5588`, and upstream
`rust-lang/rust-analyzer` `master` at `ea09c7b724`. The fork last met upstream at
`0b21bd6596` (2026-10-20), so "blaze's changes to rust-analyzer" below means
`git diff 0b21bd6596 main:vendor/rust-analyzer`.

Each claim carries a mark, as in [research/](research/README.md): **(read)** checked in the
code, a command's output or a document cited; **(reported)** someone else says so;
**(unverified)** my inference or estimate. What was fetched or measured is in
[upstream/architecture-split/](upstream/architecture-split/SOURCE.md).

## The recommendation, in short

The question was whether more of blaze should live inside the rust-analyzer subtree so that
the code outside it gets simpler. The finding is that the boundary is blurred the other way:
**most of blaze's compiler already lives inside rust-analyzer's `hir-ty` crate.**
`crates/hir-ty-codegen` (37,000 lines, Cranelift, LLVM, the wasm writer, the disk store,
tiers, the fn-cache hash) and `crates/hir-ty-borrowck` (6,000 lines) are not crates. They are
modules of `hir-ty`, pulled in by two `#[path]` lines in the fork's `mir.rs` that climb out of
`vendor/` (read: `crates/hir-ty/src/mir.rs:35-39`). `hir-ty` carries 15
dependencies for them, Cranelift and inkwell among them, and the fork cannot build without
blaze around it.

So the fix is to make the split real, in both directions:

1. **The code generator becomes a real crate, and stays in `crates/`.** Same directory,
   same files, a `Cargo.toml` of its own that depends on `hir-ty`. `hir-ty` loses the
   `#[path]`, the 15 dependencies and the `llvm`/`wasm` features. A codegen edit stops
   recompiling `hir-ty` and the six crates built on it.
2. **The borrow checker becomes a real crate inside the subtree**, at
   `crates/hir-ty-borrowck`, next to `hir-ty`. It is language semantics,
   `hir`'s diagnostics call it, and the fork is where `hir` lives.
3. **MIR building, the runtime MIR phase, the MIR passes and the interpreter stay where they
   already are, in `hir-ty`.** The interpreter runs runtime and optimized MIR, so the passes
   have to sit at or below it; a separate pass crate would have to take the interpreter and
   const eval with it. **Superseded 2026-10-28 by step 8**: they are `crates/hir-ty-mir`, the
   interpreter and const eval's evaluation with them, and `hir-ty` reaches them through
   `HirDatabase` methods the database implements.
4. **The fn-cache "code hash" stays with the code generator.** It hashes the machine code
   blaze compiled, not the source or the MIR (read: `crates/hir-ty-codegen/fncache.rs:1-12`).
5. **The disk store moves out of the code generator** into a small crate of its own
   (`crates/blaze-store`). It knows nothing about Rust, and today `remote.rs` and the store
   benchmark depend on `hir-ty` to reach it.

What this buys: one-way dependencies (the subtree never points into `crates/`, and a fork
that builds and tests alone); `hir-ty`'s `Cargo.toml` diff against upstream shrinks from 36
added lines to about 4; codegen work stops rebuilding the rust-analyzer stack; and the
interface between blaze and `hir-ty` becomes an explicit list of public items instead of
"anything in the crate".

What it does not buy, and this is the main trade-off: **moving code into `vendor/` does not
make upstream merges cheaper.** A file upstream does not have conflicts with nothing, whether
it is under `crates/` or under `vendor/`. Merge cost comes only from blaze's edits to
upstream's own files, which today are about 10,700 added and 2,300 removed lines in 169
files, 157 of which upstream itself changed in the last six months (read:
[measured/upstream-churn.txt](upstream/architecture-split/measured/upstream-churn.txt)). The
moves above do not change that number much. What does is sending the fixes that are simply
rust-analyzer bugs upstream (the parser, macro and name-resolution fixes first), and adding
new behaviour in new files rather than in upstream's. That is a separate, ongoing track
(step 6 below).

The rule for new code is in [AGENTS.md](../AGENTS.md) and
[contributing.md](contributing.md#where-new-code-goes): language semantics go in the subtree,
as a new crate or a new file there in preference to edits to upstream's files; blaze's
machinery stays in `crates/`.

## 1. What there is today

### 1.1 blaze's code, and what it uses of rust-analyzer

Line counts are of `.rs` files (read: `wc -l`).

| Code | Lines | What it is | What it uses of rust-analyzer |
|---|---|---|---|
| `crates/hir-ty-codegen` (module `hir_ty::mir::codegen`) | 36,808 | MIR to Cranelift IR (`mod.rs`, 14,036), producers (Cranelift, LLVM in `llvm*`, wasm in `wasm*.rs`), the per-function query and stitching (`fragment.rs`), patches (`patch.rs`), the disk store (`store.rs`, `store/`), stable names and cached entries (`stable.rs`, `cached.rs`), tiers (`tier.rs`), fn-cache (`fncache.rs`), debug info, unwind tables, `asm!`, targets, vtables, `TypeId`s, panic locations (`places.rs`), which crate an instance is compiled as (`home.rs`, `settled.rs`) | Everything in `hir-ty`, private items included: MIR types, `runtime_`/`optimized_`/`monomorphized_` MIR queries, `interpret_mir`, layout, `lookup_impl_method`, `field_types`, const eval, the next solver's inference context, `type_name`, `MemoryMap` internals. About 30 salsa tracked functions of its own (read: `rg '#\[salsa::tracked'`). |
| `crates/hir-ty-borrowck` (module `hir_ty::mir::borrowck`) | 6,010 | rustc's NLL and Polonius-alpha borrow checker on rust-analyzer's MIR ([borrowck.md](borrowck.md)) | `hir-ty`'s built MIR, move paths from the fork's `mir/transform/move_paths.rs`, `drop`, the next solver, variances, closure captures; private `return_slot` and `for_each_operand`. Called from `hir`'s diagnostics (`hir/src/diagnostics.rs:1314`). |
| `crates/blaze-engine`, `crates/blaze-cargo` | 31,574 | The worker (`blaze-engine`'s `lib.rs`, 11,976; `main.rs` only calls it), the cargo drop-in (`blaze-cargo`'s `cargo/`, with the plans, the sandbox, the remote store and the modules under them), daemon and sockets, the splice for ELF, Mach-O, PE and wasm (`crates/blaze-splice`; its scheduler `splicer.rs` stays in the engine), symbols from rlibs, the remote store (`remote*`), crate identity by content (`content.rs`), body-only edit detection (`outline.rs`), build inputs (`inputs.rs`, `tree.rs`, `process.rs`), doctests, tiering's sampler side (`tier.rs`), `blaze-engine mir` and `blaze-engine borrowck` | `hir`, `hir-def`, `ide-db`, `ide-diagnostics`, `load-cargo`, `project-model`, `vfs`, `syntax`; about 30 items of `hir_ty::mir::codegen`; `hir_ty::mir::print_mir_body` and `MirOptLevel` (`mirdump.rs`); `hir_ty::method_resolution::{TraitImpls, InherentImpls, impl_work}` |
| `crates/blaze-live` | 1,596 | The runtime library a live program links: the daemon's file protocol, Mach-O patching in memory, sampling | nothing |
| `crates/blaze-cache`, `-macros` | 899 | `#[blaze::cache]`: the attribute and the result store | nothing |
| `crates/blaze-wasm` (outside the workspace) | ~4,000 | The compiler built for wasm, for the playground | `hir`, `hir-def`, `hir-ty` with feature `wasm`, `ide-db`, ... |
| `crates/blaze-bench` | 64 | Microbenchmarks | `hir-ty`, only to reach the store |

### 1.2 Where blaze reaches into rust-analyzer's internals

- **The two `#[path]` modules.** Being modules of `hir-ty`, codegen and borrowck see every
  `pub(crate)` and private item of it, and nothing records which they use. A static look
  (read: `rg` over both, not compiled) finds these that are not public: `mir::return_slot`,
  `StoredPlace::walk_places`, `Terminator::for_each_operand`, `lower::is_builtin_index`,
  `monomorphization::monomorphized`, the `mir::type_name` module, `transform::move_paths`,
  and `MemoryMap::{static_at, memory_at, align_at}` (the last three added by the fork for
  codegen). The rest of what they use is already `pub`. A compile will find more; see step 2.
- **`hir-ty`'s `Cargo.toml`** carries codegen's dependencies: `cranelift-codegen`,
  `-frontend`, `-module`, `-object`, `target-lexicon`, `object`, `wasm-encoder`, `inkwell`
  (optional), `postcard`, `sha2`, `crc32fast`, `memmap2`, `rustc-demangle`,
  `rustc-stable-hash`, `unicode-width`, `libc`, `windows-sys`, and the features `llvm` and
  `wasm`, which only codegen reads (read: the `Cargo.toml` diff; `rg 'feature = "(wasm|llvm)"'`
  finds them only in `crates/hir-ty-codegen`).
- **Builds that know about it.** `tools/nix/units.nix:37-45` special-cases `hir-ty` so its source
  tree carries both directories; `crates/blaze-engine/Cargo.toml` turns on LLVM as `hir-ty/llvm`;
  the root `Cargo.toml` excludes `blaze-wasm` because its feature is on `hir-ty`; and
  `harness/unittests.py` runs codegen's 52 tests as part of `-p hir-ty` (read).
- **The store through codegen.** `remote.rs`, `remote/transfer.rs`, `remote/signed.rs` and
  `blaze-bench` import `STORE_FORMAT`, `write_file_whole`, `name_file_whole`, `pack_keys`,
  `parse_size` and the like from `hir_ty::mir::codegen` (read), though the store's own
  documentation says nothing in it knows about Rust (`store.rs:1-4`).

blaze's own crate reaches rust-analyzer through public API (`hir`, `ide_db`, `hir_def`,
`load_cargo`) and through `hir_ty::mir::codegen`. It does not reach into `hir-ty`'s
privates (read).

### 1.3 Where blaze duplicates rust-analyzer

- **`outline.rs` (672 lines)** decides from syntax whether an edit changed only bodies no
  other item can see into. rust-analyzer's `ItemTree` exists for the same purpose ("stable
  over modifications to function bodies", read: rust-analyzer's `architecture.md`), and
  per-item body lowering (`Body::relative`) already keeps other bodies' queries from
  rerunning. Whether `ItemTree` equality plus a small query for "a body others see into"
  (`const fn`, `async fn`, `-> impl Trait`, item-declaring bodies) could replace it is
  unverified; it is not part of this plan.
- **`TypeId`s and vtables.** `crates/hir-ty-codegen/type_id.rs` computes rustc's `TypeId`
  hash and `vtable.rs` rustc's vtable entries, while the interpreter in `hir-ty` numbers
  `TypeId`s by the order it meets types (read: `mir/eval.rs:230`, `eval/shim.rs:785-797`) and
  keeps its own `VTableMap`. These are rustc semantics (`rustc_middle`'s `type_id_hash` and
  `vtable_entries`) and would belong in `hir-ty` if the interpreter used them. A constant
  holding a `TypeId` does reach codegen: its memory map keeps the type of each id it gave
  (`ComplexMemoryMap::type_ids`), and the object's data gets `type_id.rs`'s hash of it there
  (read: the corpus' `type_id_in_constant`).
- **`doctest/extract.rs`** finds doc examples as rustdoc does. rust-analyzer's runnables find
  doctests for the IDE with looser rules; blaze needs rustdoc's exactly, so this is not a
  duplicate to remove (unverified judgment).

### 1.4 blaze's changes to `vendor/rust-analyzer`, by purpose

198 files, +18,656 −2,252 (read: `git diff --shortstat`). 29 files are new to the fork (7,995
lines) and cannot conflict with upstream. The other 169 are edits to upstream's files. Groups
are by path, so a file with two purposes counts once (read: from
[measured/vendor-numstat.txt](upstream/architecture-split/measured/vendor-numstat.txt)).

| Purpose | Files | Lines | Where | Upstream would take it? |
|---|---|---|---|---|
| A. MIR building as rustc builds it: drop and unwind trees, `Assert`, promotion, local order for async layouts, `let` into its pattern, two-phase borrows, let chains, match guards (borrowck-2) | 4 | +3,067 −324 | `mir/lower.rs`, `mir/lower/*` (new `match_tree.rs`) | Plausibly, piece by piece: each fixes const eval too (unverified) |
| B. The runtime MIR phase and rustc's passes: drop elaboration, asserts, packed drops, upvars, cleanup; the `BLAZE_MIR_OPT` passes; coroutine bodies; their queries | 18 | +6,357 −93 | new `mir/transform/*`, `mir/optimize/*`, `mir/held.rs`; `mir.rs`, `db.rs`, `monomorphization.rs`; `docs/mir-passes.md` | The runtime phase maybe, for the interpreter (unverified); the optimization passes unlikely, nothing in the IDE needs them (unverified) |
| C. The interpreter on runtime MIR, const eval with rustc's answers, memory maps codegen reads | 6 | +1,546 −377 | `mir/eval*`, `lib.rs`, `consteval.rs` | Partly (unverified) |
| D. Types, traits and layout as rustc has them: method resolution, the next solver, layout, drop, opaques, specialization, type names as rustc spells them | 25 | +2,134 −212 | `hir-ty/src/*`, new `mir/type_name.rs` | Mostly bug fixes, so yes, under their rules (unverified) |
| E. Incrementality: a body lowered from its own item, queries free of positions, closures keyed so an edit keeps them | 16 | +1,115 −183 | `hir-def/src/expr_store*`, `nameres.rs`, `hir-ty/src/db.rs` (closure keys) | Fits upstream's own invariant ("typing inside a function's body never invalidates global derived data", read: `architecture.md`); large (unverified) |
| F. Front-end parity: parser, `macro_rules!`, built-in macros, derives, name resolution, `tt`, `span`, `syntax` | 80 | +2,587 −685 | `parser`, `mbe`, `hir-expand`, `hir-def`, `tt`, `span`, `syntax*` | Yes, most of it: these are rust-analyzer bugs against rustc (unverified per fix) |
| G. Hooks the daemon needs: checkouts seen through another (`Files::set_view`), `include_bytes!` files, `env!` reads, the process environment, several workspaces in one database, build scripts from given output, query counting | 25 | +1,087 −299 | `base-db`, `ide-db`, `load-cargo`, `project-model`, `paths`, `proc-macro-api`, `stdx` | `include_bytes!` and `env!` tracking maybe; the rest is blaze's (unverified) |
| H. Borrowck hooked into diagnostics | 3 | +118 −2 | `hir/src/diagnostics.rs`, `ide-diagnostics` | No: upstream deleted its own borrowck on 2026-08-05 as "both hopeless and not important" (reported: [research/borrowck.md](research/borrowck.md), finding 8) |
| I. Codegen's dependencies | 1 | +36 | `hir-ty/Cargo.toml` | No |
| J. Tests | 20 | +609 −77 | `hir-ide/src/tests` | With what they test |

Where the conflicts will come from (read: [measured/upstream-churn.txt](upstream/architecture-split/measured/upstream-churn.txt),
upstream commits since 2026-04-21 to each file blaze edits): `hir-ty/src/infer.rs` 74,
`hir/src/lib.rs` 73, `hir-def/src/expr_store/lower.rs` 63, `hir-ty/src/lower.rs` 46,
`hir-ty/src/mir/lower.rs` 40 (where blaze's +2,356 −278 is), `next_solver/interner.rs` 37,
`mir/eval.rs` 32, `db.rs` 30, the root `Cargo.toml` 22, `mir.rs` 18. Upstream is also
breaking `hir-ty` into crates: step 1 moved display, extra diagnostics and tests into
`hir-ide` (merged 2026-10-12), "according to the plan outlined in Zulip" (reported:
[pr-23181](upstream/architecture-split/rust-analyzer/pr-23181-break-hir-ty-up-step-1.md)). I
could not read the plan. If a later step moves `mir/` out of `hir-ty`, the fork's 2,356-line
edit of `mir/lower.rs` and its new files under `mir/` have to follow it in one merge; git's
rename detection carries edits across a move only while the file stays half the same
(unverified for a move of that size).

## 2. The principle

### 2.1 What decides where code goes

- **Upstream merge cost comes from editing upstream's files, not from where new files are.**
  A new file under `crates/` and one under `crates/` both conflict with
  nothing (read: a three-way merge only conflicts where both sides changed the same path).
  So "inside the subtree" is not cheaper to merge than "outside"; "new file" is cheaper than
  "edit". Upstream's manifests change with every dependency bump (the root `Cargo.toml` 22
  times in six months, `hir-ty`'s 4), so dependencies added to an upstream crate's manifest
  are a conflict waiting for the next bump of a neighbouring line.
- **Dependencies point one way: `crates/` depends on `vendor/`, never back.** Today the
  subtree reaches out (`#[path]`), so the fork cannot be built or tested alone, nix needs a
  special case, and the fork's history holds a crate that only compiles inside blaze.
- **Salsa queries can live in any crate below which the database trait is defined, but not
  above what calls them.** With current salsa a `#[salsa::tracked]` function taking
  `&dyn HirDatabase` can be in any crate that depends on `hir-ty`; codegen's ~30 tracked
  functions do not need to be in `hir-ty` (read: they are free functions, not methods of
  `HirDatabase`; ide-db defines its own the same way). What must be in `hir-ty` is what
  `hir-ty` itself calls: the interpreter calls `optimize::interpreted_body` and runs runtime
  MIR (read: `mir/eval.rs:3034,3148,3444`), and const eval is a `hir-ty` query, so MIR
  building, the passes and the interpreter cannot leave it. rustc breaks the same cycle with
  query providers registered by function pointer (read:
  [compiler-src.md](upstream/architecture-split/rustc-1.99.0/rustc-dev-guide_compiler-src.md)
  "Big picture"); salsa has no such indirection, so the dependency order is the call order.
  Methods of the `HirDatabase` trait itself (the fork adds about 15 `runtime_`/`optimized_`
  ones in `db.rs`) must be in `hir-ty`.
- **Compile time follows crate boundaries.** Cargo rebuilds every crate downstream of a
  changed one. Codegen inside `hir-ty` means a one-line codegen edit rebuilds `hir-ty` (75,000
  lines of its own plus the 43,000 of the two modules), then `hir-ide`, `hir`, `ide-db`,
  `ide-diagnostics`, `load-cargo` and `blaze` (read: the dependency edges; what it cost is
  in 3.3: 2.4 times the CPU of the same edit with codegen a crate). rustc's guide gives the same reason for its split: "we try to have
  as few dependencies between crates as possible so that we don't have to rebuild as many
  crates if you change one" (read, same file). Upstream rust-analyzer is splitting `hir-ty`
  for the same reason (reported: #23181).
- **Upstreamability is real for fixes, weak for the large pieces.** Upstream deleted its
  borrowck in August 2026; it has no consumer for MIR optimization; and since 2026-08-26 it
  requires a rustc citation for analysis changes made with AI (reported:
  [pr-23233](upstream/architecture-split/rust-analyzer/pr-23233-ai-rustc-citation.md)), which
  blaze's changes, written against rustc's sources, can meet. So the reason to put borrowck
  in the subtree is that it is semantics `hir` calls, not that upstream will take it soon.
- **Keeping blaze's outside code simple** means a narrow, named interface: blaze should
  name what it needs of `hir-ty` (public items, a short list), not have all of it.

### 2.2 How rustc splits the same pieces

At 1.99.0 (read: the `Cargo.toml` files in
[rustc-1.99.0/](upstream/architecture-split/rustc-1.99.0/)):

| rustc crate | Depends on | blaze's counterpart today | After this plan |
|---|---|---|---|
| `rustc_middle` (MIR types, the query system, layout queries) | early crates | `hir-ty` (`mir.rs`, `db.rs`, `layout`) | same |
| `rustc_mir_build` (THIR to MIR) | `rustc_middle` | `hir-ty/src/mir/lower*` | same |
| `rustc_mir_dataflow` (move paths, dataflow) | `rustc_middle` | `hir-ty/src/mir/transform/move_paths.rs`, borrowck's own dataflow | same file, made public for borrowck |
| `rustc_const_eval` (the interpreter) | `rustc_middle`, `rustc_mir_dataflow` | `hir-ty/src/mir/eval*` | same |
| `rustc_mir_transform` (passes) | `rustc_middle`, `rustc_mir_dataflow`, `rustc_const_eval` | `hir-ty/src/mir/transform*`, `optimize*` | same: the interpreter runs them, so they cannot sit above it |
| `rustc_borrowck` | `rustc_middle`, `rustc_mir_dataflow`, `rustc_trait_selection` | `crates/hir-ty-borrowck`, compiled inside `hir-ty` | `crates/hir-ty-borrowck`, its own crate |
| `rustc_monomorphize` (collection, partitioning) | `rustc_middle` | in `crates/hir-ty-codegen` (`mod.rs`, `home.rs`, `settled.rs`) | same, in the codegen crate |
| `rustc_codegen_ssa`, `rustc_codegen_cranelift` (backends; cg_clif is also kept in a repository of its own and synced into rust-lang/rust, reported) | `rustc_middle`, `rustc_mir_transform`, `rustc_symbol_mangling` | `crates/hir-ty-codegen`, compiled inside `hir-ty` | `crates/hir-ty-codegen`, its own crate |
| `rustc_interface`, `rustc_driver` | everything | `crates/blaze-engine` | same |

The match is close, with one deliberate difference: rustc keeps the passes in their own crate
because its interpreter does not need them; blaze's does. And rustc's guide is candid that
the split is for compile time and organisation, not a law: "Ideally there would be fewer,
more cohesive crates" (read).

### 2.3 The rule

New code goes in the subtree when it is the language's semantics: building MIR and the
passes over it, borrow checking, const evaluation, layout, rustc-exact identities (`TypeId`,
vtable layout, type names), and any hash of source or MIR as a salsa query. These are what
rustc keeps in `rustc_middle`, `rustc_mir_build`, `rustc_mir_dataflow`, `rustc_const_eval`,
`rustc_mir_transform` and `rustc_borrowck`. Inside the subtree it goes in a new crate, or a
new file declared with one line, rather than in edits through upstream's files, and a fix to
an upstream bug goes upstream.

New code stays in `crates/` when it is blaze's machinery: code generation and its backends,
object, debug-info and unwind writing, which crate an instance is compiled as, the splice,
the daemon and sockets, the cargo drop-in, the store and the remote, tiers and sampling,
fn-cache (it hashes compiled code), the live runtime and the harnesses.

The subtree never depends on `crates/`.

## 3. The target layout

    crates/
      hir-def, hir-expand, base-db, ...   upstream, with blaze's fixes (groups E, F, G)
      hir-ty                              upstream + MIR building, runtime phase, passes, interpreter (A-D)
                                          loses: #[path] modules, 15 dependencies, features llvm and wasm
                                          gains: a short list of items made public for the crates below
      hir-ty-mir             NEW (step 8) hir-ty/src/mir's lowering, passes, monomorphization, interpreter
      hir-ty-borrowck        NEW          from crates/hir-ty-borrowck; depends on hir-ty
      hir-ide, hir, ide-*                 upstream; hir's diagnostics call hir_ty_borrowck
    crates/
      hir-ty-codegen         NOW A CRATE  same directory and files; depends on hir-ty, blaze-store
                                          features llvm, wasm; Cranelift, inkwell, object, ... move here
      blaze-store           NEW          from hir-ty-codegen/store.rs, store/, the file helpers
      blaze                              depends on hir-ty-codegen, blaze-store
      blaze-live, blaze-cache, blaze-cache-macros, blaze-wasm, blaze-bench

The name `hir-ty-codegen` stays: renaming the directory would move every file under
in-flight branches for no gain. Its crate root is `mod.rs` (`[lib] path = "mod.rs"`), so no
file moves at all.

### 3.1 What changes and what gets simpler

| Change | Lines touched (estimate, unverified) | What gets simpler |
|---|---|---|
| Codegen as a crate | ~290 import lines in 26 files (`crate::mir::codegen::` → `crate::`, `crate::`/`super::` → `hir_ty::`, `hir_ty::mir::`; read: `rg -c`), 29 paths in `crates/blaze-engine` (`hir_ty::mir::codegen` → `hir_ty_codegen`), ~10-20 visibility words in `hir-ty`, a new `Cargo.toml` | `hir-ty/Cargo.toml` diff vs upstream from +36 to about +4 lines; the `#[path]` lines go; `tools/nix/units.nix`'s `hir-ty` special case (9 lines) goes; `blaze`'s `llvm` feature points at its own dependency; a codegen edit rebuilds two crates instead of eight; codegen's use of `hir-ty` becomes a list someone can read |
| Store as a crate | ~2,560 lines moved unchanged, ~15 import lines | `remote*` and `blaze-bench` no longer depend on `hir-ty` for files and packs; the store's tests and fuzz run without building a compiler |
| Borrowck into the subtree | 6,010 lines moved unchanged, ~70 import lines, `hir/Cargo.toml` +1, `hir/src/diagnostics.rs` 1 line, `mir.rs` −2 lines | The fork builds and tests alone; the last `#[path]` goes; a borrowck edit no longer rebuilds `hir-ty` and `hir-ide` |

Lines removed outright are few (the two `#[path]` lines, ~32 manifest lines, the nix special
case); the gain is in what each crate can see and what each edit rebuilds, not in line count.

### 3.2 What should not move, and why

- **The code generator into the subtree.** It is Cranelift, LLVM and object files; upstream
  would never take it, the fork would carry 37,000 lines that are not rust-analyzer, and
  moving it would not save a single merge conflict.
- **The MIR passes into their own crate** (`hir-ty-mir-transform`). The interpreter calls
  them, so the crate would have to sit below the interpreter, which means taking
  `mir/eval*` and const eval out of `hir-ty` too, and those are called by `hir-ty`'s layout and
  inference queries. Revisit if upstream's `hir-ty` split moves MIR into a crate of its own;
  then put the passes there. Done otherwise in step 8: what `hir-ty` asks of them (the MIR body
  queries, `interpret_mir`, `opaque_of_impl`) are required `HirDatabase` methods, as upstream's
  `type_name` is, which `hir-ide`'s `impl_hir_database!` implements by calling `hir-ty-mir`.
- **The fn-cache hash into `hir-ty`.** It is a hash of compiled code; it needs the code
  generator. A hash of a function's MIR and its callees, if one is ever wanted, would be a
  `hir-ty` query in a new file.
- **`ContentId`, stable names, `home.rs`, `settled.rs`.** They decide how blaze names,
  shares and caches compiled code across checkouts and runs; nothing in the language says so.
- **The daemon's hooks in `base-db`, `ide-db`, `load-cargo`, `project-model` (group G).** They
  are edits to upstream types and must stay where the types are. They can be made smaller
  (step 6), not moved.
- **`type_id.rs` and `vtable.rs` now.** They are semantics, but only the code generator uses
  them. Move them into `hir-ty` (as new files) when the interpreter needs rustc's `TypeId` or
  vtable layout, in the same change that makes it use them.

### 3.3 What the rebuild costs, measured

`cargo build --release -p blaze-engine` after a one-line edit, twice per file, on the M5 Max while
other lanes kept its load average between 45 and 110 (read:
[measured/rebuild-times.txt](upstream/architecture-split/measured/rebuild-times.txt), which
has each run, its load and the crates compiled). The load makes wall time noisy; CPU time
(user plus system) is what the edit costs and barely moved between runs.

| Edit in | Before step 2: wall, CPU (s) | After step 2: wall, CPU (s) | Crates compiled, before → after |
|---|---|---|---|
| `crates/hir-ty-codegen/mod.rs` | 186, 152; 366, 369 | 80, 113; 146, 145 | 7 → 2 (`hir-ty-codegen`, `blaze`) |
| `crates/hir-ty-codegen/store.rs` | 146, 118; 346, 338 | 100, 103; 146, 155 | 7 → 2 |
| `crates/hir-ty-borrowck/check.rs` (still a `hir-ty` module in both) | 132, 181; 377, 380 | 133, 130; 329, 330 | 7 → 8 (`hir-ty-codegen` joins) |
| `crates/blaze-engine/src/main.rs` | 47, 22; 91, 86 | 51, 37; 88, 90 | 1 → 1 |

After steps 3 and 4 (`after-step3` in the file, at a lower load, 26 to 42):

| Edit in | Wall, CPU (s) | Crates compiled |
|---|---|---|
| `crates/blaze-store/src/lib.rs` | 45, 52; 138, 145 | 3 (`blaze-store`, `hir-ty-codegen`, `blaze`) |
| `crates/hir-ty-borrowck/src/check.rs` | 30, 55; 148, 153 | 6 (`hir-ty-borrowck`, `hir`, `ide-db`, `load-cargo`, `ide-diagnostics`, `blaze`) |

So a code generator edit costs 2.5 times less CPU (366 to 146 s) and about 1.7 times less
wall time under this load, and a borrow checker edit, now a crate under `hir`, 2.5 times less
(379 to 150 s): neither rebuilds `hir-ty` and `hir-ide` any more. The floor is `blaze`
itself, about 88 s of CPU. A store edit still rebuilds the code generator, which uses it;
what step 3 bought is that the store's tests and benchmark no longer build a compiler.

## 4. Migration plan

Each step is one branch off `main`, merged `--no-ff` once its gate passes, and builds on
its own. A step that moves files does so in a first commit with nothing but the move (`git
mv`, 100% similar), so that branches still editing the old paths merge through git's rename
detection; the edits that make it build come in the commits after.

The gate named below is, unless a step says otherwise:
`cargo test --release -p hir-ty -p hir-ty-codegen -p hir-ty-borrowck -p hir-def -p parser -p syntax -p blaze-engine`
(the crates that exist at that step; `harness/unittests.py` runs them in one `cargo test`),
`cargo test --release -p hir-ide` failing exactly the 18
known tests, the corpus under Cranelift and LLVM (with `--layouts`), `mixfutures`, `growth.py`,
`movedlines.py`, `borrowck_ui.py` at its floor, and `cargo fmt --all -- --check`
([harness.md](harness.md)).

| Step | Change | Verification beyond the gate | Effort (unverified) |
|---|---|---|---|
| 0 | Measure: on the dev host, time `cargo build --release -p blaze-engine` after a one-line change in `crates/hir-ty-codegen/mod.rs`, in `crates/hir-ty-borrowck/check.rs` and in `crates/blaze-engine/src/main.rs`; write the numbers into this document. Done, with `store.rs` too, before and after step 2: 3.3 | none | done |
| 1 | **Rule and docs.** Done: the AGENTS.md rule and the "Where new code goes" section of contributing.md are on `main` (`1c8d34b056`) | none | done |
| 2 | **Codegen becomes a crate.** Done on `codegen-crate`, 2026-10-21, in two commits (no file moved, so no rename commit). The compile found what the text search in 1.2 missed, in the other direction too: `hir-ty` itself called into the code generator, its interpreter and MIR lowering asking `Target::current().word()`, `body_checks_overflow` and `placed` (where `line!()`, `column!()` and `file!()` are as written). Those semantics moved into three new fork-owned files of `hir-ty`: `mir/target.rs` (the target's word size, which the code generator's `Target::select` sets), `mir/overflow.rs` and `mir/location.rs` (about 270 lines out of codegen's `mod.rs`). Made `pub` in fork-owned code: `is_builtin_index`, `monomorphized`, the `type_name` module and its functions, `loaded_crates`, `MemoryMap::{static_at, memory_at, align_at}`. Four upstream lines changed one word each: `Generics::{iter_self, len_parent}`, `layout::struct_tail_erasing_lifetimes`, `StoredPlace::walk_places`. As step 4 did, `return_slot` is copied, `Operand::span()` used, and `ParamEnv`'s private `clauses` read through `caller_bounds()`. `hir-ty/Cargo.toml`'s diff against upstream went from 36 added lines to 5 (`unicode-width`, now `location.rs`'s, `cfg` and `stacker`). The crate's root `mod.rs` aliases `ra_ap_rustc_*` to the names `hir-ty` gives them; `publish_crates.py` stages it as `blaze-hir-ty-codegen` (any crate of `crates/` not named `blaze*` gets the prefix); `blaze-wasm` turns on `hir-ty-codegen/wasm` and selects its target before lowering MIR; `harness/unittests.py` and CI's fmt run `-p hir-ty-codegen` (45 of its 52 tests on macOS; the rest need LLVM, Windows or Linux); `tools/nix/units.nix` has no special case left | rebuild times again (3.3); `cargo build --release -p blaze-engine --features llvm`; `blaze-wasm` builds | done |
| 3 | **Store becomes a crate.** Done on `store-crate`, 2026-10-21: commit 1 the `git mv` alone of `store.rs` (now `crates/blaze-store/src/lib.rs`), `store/acl.rs`, `store/bench.rs` and the corpora `pack` and `name`, and of `fuzz.rs`, the inputs and counting allocator the store's and codegen's reader tests share, as `crates/blaze-fuzz` (a dev-dependency only, since it sets the global allocator); commit 2 the manifests and imports. What stays in the code generator is what knows about code: the stored entry (`cached.rs`), Cranelift's incremental cache over the store, and the entry key the store bench uses (`hir_ty_codegen::bench::entry_key`). `crc32fast`, `memmap2`, `libc` and `windows-sys` moved with it. `blaze`'s remote, signing, keeper and daemon, and `blaze-bench`, import `blaze_store`; the store's tests run as `-p blaze-store` without building a compiler | `cargo test --release -p blaze-store`; `remote.py`; the store bench builds | done |
| 4 | **Borrowck into the subtree.** Done on `borrowck-3`, 2026-10-21, as the plan said: commit 1 the `git mv` alone (100% similar), commit 2 the crate. What the compile needed of `hir-ty` was smaller than 1.2's text search (read: the compile errors): the four `for_each_operand` methods (fork-owned) made `pub`; `Operand` gained a `span()` accessor instead of widening upstream's private field; `return_slot` copied into the crate; `GenericPredicates` was already public at the crate root; `transform::move_paths` was not needed. `hir` depends on the crate and calls `hir_ty_borrowck::borrowck`. `tools/nix/units.nix` lost the borrowck half of its special case (the code generator half goes with step 2), `tools/nix/units/daemon.json` regenerated (one package added), `harness/unittests.py` runs `-p hir-ty-borrowck` (it has no unit tests; its tests are `borrowck_ui.py`), `publish_crates.py` stages it as `blaze-hir-ty-borrowck` unchanged (before, the published `hir-ty` lacked the checker's files). The fork still cannot build alone until step 2 removes the `#[path] codegen` line, so step 5 waits for step 2 too | `borrowck_ui.py` scores unchanged in both modes; `blaze-engine borrowck --project` on ripgrep, fd, bat, hyperfine, just, axum and nushell finds nothing; the corpus, `mixfutures`, `hir-ide`'s 18 known failures, `cargo test --release -p blaze-engine` | done |
| 5 | **Fork check.** Withdrawn 2026-10-28 with the fork (one workspace). Was done on `fork-check`, 2026-10-21: CI's job `fork alone` copies `vendor/rust-analyzer` out of the repository, which is the tree `fork sync` pushes, and runs `cargo test -p hir-ty -p hir-ty-borrowck && cargo check -p hir` there; `ci` needs it. Copying rather than cloning the fork catches a dependency on `crates/` before it merges, not after. The fork alone needed two fixes: its workspace patched `rustc_abi` from `lib/rustc_abi`, a directory the fork never had, and now takes it from `indexable-inc/rustc_abi` `main` (the subtree of `crates/rustc_abi`, identical to it at `8f6db283fd`; blaze's own workspace still patches it by path); and its `Cargo.lock` was brought up to date (it lacked `hir-ty-borrowck`, `stacker`, `unicode-width`) | the job's commands, run in a copy of the subtree | done |
| 6 | **Shrink the edit surface.** Withdrawn 2026-10-28: nothing is sent upstream any more. Was: send group F upstream in small pull requests, parser and `macro_rules!` first, each with the rustc source it follows (upstream's AI rule); when one merges, the next `vendor/subtree.py upstream` takes upstream's version. Move group G's additions to new files where they are additive (`Files`'s view, include and env fields behind one field of a type defined in a new `base-db` module). Watch upstream's `hir-ty` split and put new MIR code where upstream's plan will put MIR | each pull request under upstream's CI; the gate here when each comes back through a merge | a day per batch, spread out |
| 7 | **Optional.** `type_id.rs` and `vtable.rs` into `hir-ty` when the interpreter needs them; a look at whether `outline.rs` can be an `ItemTree` comparison | the corpus' `TypeId` and `dyn` cases; `growth.py`, `movedlines.py` for `outline.rs` | a day each |
| 8 | **MIR's passes become a crate.** Done 2026-10-28 (crate split 4c), in three commits: what `hir-ty` asks of them moves to `HirDatabase`, the `git mv` alone, then the crate. `crates/hir-ty-mir` has `mir/{lower,eval,optimize,transform}*`, `monomorphization`, `pretty`, `type_name`, `storage`, `location` and `overflow`; `hir-ty`'s `mir.rs` keeps the types and gains the errors (`MirLowerError`, `MirEvalError`, which `ConstEvalError` contains and which contains it), `VTableMap`, `MirOptLevel` and `is_by_move_coroutine`; `held.rs` and `target.rs` stay, as layout reads them. The MIR body queries, `interpret_mir` and `opaque_of_impl` are required `HirDatabase` methods (upstream's pattern for `type_name`), implemented in `hir-ide`'s `impl_hir_database!` for `RootDatabase` and `TestDB`, the only databases. `hir-ty-mir`'s root imports `hir_ty::*` and its `mir` re-exports `hir_ty::mir::*` with the passes as children, so their `crate::` and `super::` paths did not change; the `hir-ty` items they use are `pub`, and `MirBody::frame_storage_conflicts` is the trait `FrameStorage`'s. Downstream, `hir_ty::mir` is `hir_ty_mir::mir` | the gate (3488 tests, the same as `main`); a pass edit 54 to 40 s, an edit of `hir-ty` no slower ([tools/bench/results/2026-10-28-crate-split](../tools/bench/results/2026-10-28-crate-split/README.md)) | done |

### 4.1 Sequencing around the branches in flight

Branches ahead of `main` on 2026-10-21 (read: `git rev-list --count origin/main..<branch>`
and the paths each changes; the many older branches whose history predates the subtree
import are left out):

- **`borrowck-2`** (9 commits: `crates/hir-ty-borrowck`, MIR lowering in `hir-ty`) and the
  `borrowck-3` that follows it: step 4 waits for `borrowck-2` to merge and is best done by the
  borrowck lane itself, as the first commit of `borrowck-3`, so no borrowck branch ever
  rebases over the move. If `borrowck-3` is already under way, the move's pure-rename commit
  lets its edits follow; only edits to the `use` blocks at the top of each file conflict.
- **`cold-start-2`** (`crates/blaze-engine`, two codegen files), **`runtime-parity`** (MIR lowering
  and codegen) and the **tiers / live-latency** branches: step 2 moves no file, so they
  conflict with it only where they edit a `use` block of a codegen file or one of the `hir-ty`
  items made public. Merge whichever of them is ready first; land step 2 between merges, not
  across a running one.
- **`one-build`** and the CI fixes (`.github`, `flake.nix`, `nix/`): step 2 commit 3 and step
  4 commit 3 touch CI test lists and `tools/nix/units.nix`. Let those branches merge first, then do
  the steps.
- Order: 0, then 2 (the biggest gain, no moves), then 3 (independent), then 4 at the borrowck
  handover, then 5. Steps 6 and 7 run alongside whenever convenient.

### 4.2 Fork sync and upstream merges afterwards

None since 2026-10-28: the forks are archived, and upstream rust-analyzer is not merged in.

## 5. What I did not check

- The list of private items in 1.2 is from a text search; steps 2 and 4 compiled the real
  lists, which are in their rows of section 4.
- Upstream's plan for the rest of the `hir-ty` split (the Zulip thread) was not readable
  without a login.
- Whether upstream would take groups A to E is a judgment; nobody upstream was asked.
- `borrowck-3` and `runtime-parity` were not found as branches locally or on `origin`; what
  they will touch is inferred from their names and from `borrowck-2` and the parity branches.
