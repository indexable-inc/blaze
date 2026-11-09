---
status: plan
summary: The one definition of code identity in blaze (crates/hir-ty-identity). Compile identity is what one instance's generated code depends on; behaviour identity (CodeId) is what running it can depend on. Both are Merkle graphs with recursion hashed as one strongly connected component. Every other hash of code ports onto them and is deleted.
date: 2026-11-02
---
# Code identity

`crates/hir-ty-identity` is the one place blaze says what a piece of code is: a hash that is the
same in every process, on every machine and in every checkout of the same source, and that
changes when, and only when, what it names changes (AGENTS.md rules 18 and 22). It has two
notions, each named for what it means:

| | Compile identity | Behaviour identity |
|---|---|---|
| Entry | `instance_identity(db, &Instance, env, Compiled)` | `code_id(db, &Instance, env)`, `fn_code_id(db, ty, env)` |
| Value | `CompileIdentity(ContentId)` | `CodeId(ContentId)` |
| Means | the code generator is given the same MIR and facts, so with the unit's template key (settings, built-in callees, devirt world) its output for one serves the other | running it can do the same things |
| A generic body's parameters | a type by its fact (layout and shape: `Vec<&A>` and `Vec<&B>` are one), a constant by value | full identity |
| A plain callee | by its own compile identity, where the generic body names a parameter; else by symbol | by its own behaviour identity |
| Inlined bodies | their code and `written_checked` (their instances: the template key) | n/a (each callee is its own node) |
| Built-in callees (`#[inline(always)]` of another crate) | the template key (`Described::built_in`) | their behaviour identity |
| MIR options | every level, UB checks and the rest (`Compiled`) | out: a debug and a release build of a step have one CodeId |
| Devirt world, overflow checks | the template key (`Inlined::devirtualized`, `Settings`) | each instance's effective overflow bit (below) |
| Recursion | the instance itself: shares; a callee with strictly shallower arguments, or of another body whose type and constant arguments are an order-preserving sub-list of the instance's own, compared by value with free regions erased in both keys: asked; any other callee (deeper arguments, or as deep with anything new, such as a literal or a projection's value that is none of the instance's arguments, or a moved argument), a cycle (`Unshared::Recursive`) and arguments nested past 128 (`Unshared::TooDeep`): the callee by its arguments, the last two compiled per instance. A chain of asks is at most (128 + 1) × arity × bodies long | one strongly connected component |
| Used by | codegen's instance and store keys, ICF | `#[blaze::cache]`, the kernel's step identity, `core::reflect::fn_info(f).code_id` |

Positions are a presentation layer: a body's code is blaze-items' code hash, its bindings' names
and its span-table indices (`hir_ty_share::code`), never a line or column. No key reads a path:
crates by content (name, version, origin, edition, dependencies, cfg), items by AST id within
their file. Comments, formatting and item order are not in either identity.

## What each instance's own part reads

The facts of its monomorphized body (`facts.rs`), in MIR order: each local's type and the types
its fields hold (normalized, so a projection through an impl is its layout), the type of every
place prefix it names, each constant's value and the memory it points to (pointees by content:
statics by key in compile identity and by value in behaviour identity, function pointers by
callee, vtables by their methods), each call's resolved callee, every drop and unsizing. A type
is its `hir_ty_share::identity` (an ADT by key, interface, layout and drop impls), hashed once
per body and named by index after.

- **Compile identity** is the share walk, moved into `hir-ty-identity/src/instance.rs` on
  2026-11-02 (lane identity-port). It does not read the facts above: per generic body and
  options set it lists the sites where the body names a parameter (`sites`), and per instance
  records only what is there (a type's fact, a constant's value, a callee's compile identity, a
  drop key, an intrinsic's class), under every MIR options of `Compiled { lang, other }`, plus
  the code of every body MIR inlining built in. What it leaves out the unit's template key
  already covers: settings, built-in callees, inlined instances and the devirt world.
- **Behaviour identity** reads the runtime MIR and adds, per instance, whether its arithmetic
  checks overflow: `body_written_checked(body) && checked`, where `checked` is the step's
  compiling crate's `-C overflow-checks`, as rustc's code generator drops an optional overflow
  check its crate does not run. Its edges are everything dispatch says the instance can run:
  calls, `Fn*` of closures and function items, drop glue, reified pointers, vtables made by
  unsizing and held by constants, an `async` block's `poll`. It stops at three leaves, each by
  key and interface: a `#[blaze::pure(trusted = "..")]` function (with its own code, following
  the closures it is given), a method of a trusted trait (the kernel's `ctx` door), and a
  foreign function (with its link name). A root whose parameters hold a function pointer or a
  non-door trait object is refused: what it runs through them is not in the walk.

## Merkle graph, recursion as one component

Each instance's identity is its own query, so it is hashed once and shared by every root that
reaches it. Per graph there are three queries (`scc.rs`, the `Graph` trait):

1. `local(node)`: the node's own content and its callees in site order (`walk::summary`,
   `instance::own`).
2. `cyclic(node)`: asks every callee's `cyclic` and returns `false`, with `cycle_result = true`.
   blaze_query gives every member of a query cycle its fallback (`CycleKind::Result`), so this
   is `true` exactly on nodes of a cycle or a self-loop, whatever order queries ran in.
3. `id(node)`: if not cyclic, `H(content, callee ids)` as a component of one. If cyclic, a
   structural Tarjan over the cyclic nodes it reaches finds its strongly connected component;
   every callee outside the component is asked for its `id` (it cannot reach back, so no query
   cycle forms); the component is hashed as one unit.

A component's members are put in a canonical order: each member's local hash (its content, the
ids of its callees outside, a placeholder for each inside), ties broken by a stable name (body
key and arguments' content identity, never an interned id). The component hash is its members'
contents and edges in that order, an inside callee by its place; a member's id is the component
hash and its place. A node on no cycle is the same formula with one member, so a `cyclic` that
is spuriously `true` (every query on a const-eval cycle is a member) changes the path taken,
never an identity.

## Code that reads its own identity

`fn_info(f).code_id` is answered by const eval (`docs/v2/plan/design/comptime.md`, "Function
reflection"), so it is a constant in the caller's MIR and its value is in the caller's identity
through "each constant's value". A function whose constant asks for the id of code that runs it
depends on itself: evaluating the constant re-enters `summary` (or `own`) through
`code_id_of`. Every query of this crate on that cycle has `cycle_result` = `SELF_READ`, a typed
`Unidentified`, never a fixpoint and never a panic. hir-ty's const-eval queries on the same
cycle give their own fallback, so which error text the user sees depends on which query the
cycle was entered at; that both are errors does not.

## Ports onto it, then deletion

Every other hash of code ports onto these two and is deleted, each with a `.clippy.toml` ban
naming its replacement (rule 11). The call sites are in the identity lane's handoff
(`/tmp/blaze-lanes/v2-identity.md`); the shape of each port:

1. **`hir_ty_share::share_key` becomes compile identity. Done 2026-11-02.** `share_key`,
   `ShareKey`, `ShareInstance`, `ShareDef`, `BodyOptions`, `OptionsSet` and `walk.rs` are gone
   from `hir-ty-share`, which keeps the building blocks (`Cx::fact`, `identity`, `code`, `drop`,
   `classify`, `Unshared`), each ban in `.clippy.toml` naming the replacement. Mutual recursion
   stays `Unshared::Recursive` for now (no component), and constants and statics are not
   subjects. An identity reads only its instance's key, never who asked: no identity asks a
   callee deeper than itself, or as deep unless it is another body passing on an
   order-preserving sub-list of the instance's own arguments (2026-11-02, round 6; round 5's
   rule, "made of the instance's arguments' parts", still let a chain count 2^k steps through k
   digits held in separate parameters). Every key has its free regions erased (round 7: a
   callee's lifetimes put through a permutation would otherwise count lcm-of-cycles steps, as
   lifetimes are no part of the sub-list check). Along a chain of asks depth never rises, at one depth
   the arity never rises, and at both the arguments are equal, so a chain is at most
   (`SHARED_DEPTH` + 1) × arity × bodies asks long, from its keys alone; a root past `SHARED_DEPTH` is
   `Unshared::TooDeep`, per instance; and rustc's recursion limit (a body repeated past the limit along rustc's
   depth-first path, drop glue counting a quarter, held to the head's crate's
   `#![recursion_limit]`, a non-generic non-`#[inline]` function of another crate heading a
   path of its own) is checked by the build's walk (`blaze-codegen` `driver/recursion.rs`),
   never by the identity (2026-11-02, identity-port rounds 3 to 5). The plan as first written follows. The share walk is the more complete
   account of what generated code reads (shapes, not type names, so `Vec<&A>` and `Vec<&B>`
   share; one walk per distinct body over the origin's `BodyOptions`; shims). It moves into
   `hir-ty-identity` as the compile identity's facts, and this crate's additions go with it:
   inlined bodies' code, built-in callees as Merkle edges, place-prefix types, constants'
   pointees, the devirt world. A callee is named by its compile identity, which is its symbol
   once symbols are share-keyed, and mutual recursion is a component instead of
   `Unshared::Recursive`. `Subject` gains the shims; `per_instance` becomes a typed kind of
   `Unidentified`. `HirFrontend::shared_args`, `code_of`, `identity` and `bound` build the
   `InstanceKey` from it.
2. **`#[blaze::cache]`'s code hash becomes behaviour identity. The hash is done 2026-11-02**:
   `cache_code` answers `H(FORMAT 2, triple, nightly, checked, code_id(closure))`, and the
   walk's machine-code hash is deleted; the walk stays only for the purity rules until
   `hir-ty-pure` takes them, and `__blaze_cache_code` is still answered by the code generator
   with a `u128`. Left: the macro asks
   `fn_info(closure).code_id` in const eval instead of `__blaze_cache_code::<F>` answered by the
   code generator; the purity rules move to `hir-ty-pure`'s check
   (`/tmp/blaze-lanes/v2-pure.md`, the fncache table); the cache key is the CodeId plus the
   target triple; `blaze::__private::Code` holds 32 bytes.
3. **The kernel's `CodeId` is behaviour identity.** A step's code is
   `fn_info(Step::compute).code_id`, converted by `.0`. A dev template's is the CodeId of the
   code generator's template entry, once blaze builds blaze with `blaze_comptime`.
4. **`Toolchain::id` leaves every key.** Generated code depends on the code generator's
   behaviour, not its executable's bytes: `Settings`, template keys, premap keys and cargo's
   record inputs take the code generator's CodeId and the pinned nightly in place of the
   executable hash, so a blaze rebuild that does not change codegen keeps every template.
   Done at package granularity (2026-11-02): `Toolchain::id` is `H(ID_DOMAIN 5, NIGHTLY,
   CodegenCode, TargetData::content)`, where `CodegenCode` is the content identity of every
   package the closure's roots are built from (`hir-ty-codegen`, and `blaze-item-hash`, whose
   `BodyFacts` is installed into the front end through a `dyn` hook; registry packages by lock
   checksum, checkout ones by their files), the workspace manifests and cargo's configuration
   files (the root's, those above it and `$CARGO_HOME`'s), `harness/toolchain`, LLVM by its pin
   (llvm.json's version, commit and the sha256 of the release for `TARGET` and `HOST`, and the
   prefix's `blaze-llvm.json` when set, checked by vendor/llvm-sys/pin.rs, which the build
   script includes), `$RUSTC -vV` and the build's `TARGET`, `PROFILE`, cfgs, features,
   rustflags and the variables the closure's build scripts read. blaze as `rustc` ends its
   `-vV` with `blaze-identity: <its code identity>`, so a blaze built by blaze is another code
   than one rustc built. blaze-engine's build script (`crates/blaze-codegen-code`) works it out
   from `Cargo.lock` and the manifests alone (no cargo, no network, nothing written), refuses
   when a file, lock or manifest it read or a package directory it listed changed while it
   ran, and hands it over with `hir_ty_codegen::v2::set_code`. The window between the script
   and the compiles of the closure's crates is accepted for direct builds in a tree being
   edited, in both orders: the shared runner and land.py build from snapshots nothing writes
   to, and working the identity out again over the exact tree of a checkpoint or landing
   closes it (#1619). The target data (data layout) is read from blaze-units'
   recorded spec, past the closure, so it is in the id itself. Tests hold it to every file and
   variable rustc's dep-info says those crates read, to what the closure's build scripts say
   they read (`rerun-if-changed`, `rerun-if-env-changed`), and every implementation of a
   front-end hook past the closure to a reviewed list. No
   executable hash is left in a template key.
   The front end's stores take a second identity of the same kind (2026-11-03):
   `blaze_codegen_code::FRONT_END` (`BLAZE_FRONT_END_CODE`), of every package the front end's
   roots are built from (`hir-def`, `hir-ty`, `blaze-diagnostics`, `blaze-items`, whose `Goals`
   is installed as `GoalFacts`, `blaze-item-hash`, `blaze-lower`, `blaze-macro-eval`) and of how
   blaze was built. blaze-engine's build script works it out in the same walk as `CodegenCode`
   (`emit_all`: one read of the workspace, one `$RUSTC -vV`, each package hashed once) and the
   program hands it over with `blaze_cargo::serve::set_front_end_code`. It keys def-map images
   and the check outcomes, bodies and constants served beside them, the goal store, the item
   store and kept proc-macro expansions, so an edit to the code generator, the CLI or the lints
   keeps every one of them. Its tests are the code generator's, per identity: dep-info coverage
   from blaze-cargo's target directory (`blaze_codegen_code::coverage`), and reviewed hooks,
   where a hook is a trait the identity lists or one its closure defines and names `dyn`,
   implemented past the closure in a package the program links and built from the trait's
   package; only the files a crate is compiled from count, read as tokens. A finer key for the
   images alone (`hir-def` and what it is built from) is not clean: an image holds macro
   expansions, and blaze-macro-eval, which expands, is built from `hir-ty` and `blaze-lower`.
   What FRONT_END leaves out is in the keys beside it: a proc macro run in a wasm module keys
   the crates that call it by its crate's code and the module (`ProcMacroExpander::code`), since
   the module is the code generator's; a kept check's options are made of its outcome key's
   inputs (`blaze_diagnostics::cache::Inputs::options`), not by blaze-cargo; and every package
   that depends on one of the closure's, by any edge of the lock, is an input (its lock checksum
   or its manifest), since cargo unifies features across what it builds together.
   Cargo's unit records and the standard library's shared directory (which holds std units'
   records alone) key by `RecordMakers` (`blaze_cargo::serve::record_makers`): `FRONT_END`,
   `CODEGEN` and `RECORD`, the code identity of blaze-cargo and every package it is built from
   but those `RECORD_CUT` names (the explorer and its page, the plane's history, the client's
   rendering), each cut with why no record depends on it and the root files that name it, held
   both ways by a test (`tests::cuts`); a cut that a closure package other than a root is built
   from is refused. Never the executable's bytes: an edit of blaze-engine or a cut package keeps
   every record. blaze-cargo's `the_record_identity_hashes_every_input_what_makes_a_record_read`
   holds `RECORD` to dep-info like the front end's. Still one input: blaze-cargo whole, the CLI
   and daemon with it, until record-making is a crate of its own. Accepted, as for the other two:
   the C compiler, its flags and SDK for C in the closure (ring among it), and which `--features`
   blaze itself is built with beyond what reaches a build script (`CARGO_FEATURE_*`). A record
   miss reruns the unit against images and stores that stay. The trait solver has no identity of its own: hir-ty and
   the vendored solver crates are in `FRONT_END`'s closure, and the goal and item stores' solver
   version is a domain over it, so no build script runs in the front end (a `SOLVER` identity
   worked out by hir-ty's own build script reran hir-ty and its 36 dependents on every
   `Cargo.lock` edit; removed 2026-11-03). Where there is no checkout (a published crate, a Nix
   unit cut down to its package), the program's build script reads the shipped tree
   `.github/scripts/publish_crates.py` writes beside the package (`ship-code-identity`, one
   `shipped-*-code.txt` per identity: what the checkout alone decides, finished with
   `$CARGO_HOME`, LLVM, the environment and the compiler), and refuses, naming both, when it
   has neither. What follows replaces the package grain with the code
   generator's CodeId. The steps, in order:
   1. Lane comptime: const eval answers `fn_info(f).code_id` from `hir_ty_identity::code_id`
      through a table registered in `hir_ty::providers` with `inventory::submit!`; no table is
      a typed `MirEvalError` naming `f`, never a zero id.
   2. Lane pure: the code generator's entry and the traits it calls the front end through
      (`HirFrontend` and the database) become trusted doors, so the CodeId covers what the
      code generator runs. It must also cover the front end and MIR builder, since they make
      the code too: either they are in the walk, or their own CodeId is a second part of the
      key. A door that leaves either out keys stale code (rule 18).
   3. blaze builds blaze with `#![feature(blaze_comptime)]` in `hir-ty-codegen`.
   4. `toolchain::of` sets `id` to `H(ID_DOMAIN 6, NIGHTLY, fn_info(<entry>).code_id, TargetData)` and
      deletes `CodegenCode`, `set_code` and `crates/blaze-codegen-code`; a CodeId that can't be had
      stops the build with its error. Every reader of the id then keeps working as is: they
      take `Toolchain` or `Settings` whole (blaze-global `public.rs`, blaze-cargo
      `serve/unit/compile.rs`, blaze-codegen `driver.rs`, `later.rs`, `kernel.rs`, blaze-premap
      `key.rs`), which the step checks call site by call site.
5. **`hir-ty-pure`'s script identity** (`identity::of`, `content`) is behaviour identity of the
   script's root.

`blaze_codegen::content::{code_id, data_id}` hash machine code after codegen (ICF of output),
which is not code identity; `content::code_id` is renamed so the name means one thing.

## Property for the edit fuzzer

The verify lane's Hegel edit fuzzer
(`crates/hir-ty-codegen/v2/edits/identity.rs`, `docs/v2/plan/design/verify.md`) holds:

- An edit to comments, whitespace, formatting or item order keeps every instance's compile
  identity (debug and release units) and every root's CodeId.
- An edit that changes behaviour (a literal, an operator, a branch, a callee's body, a field
  added to a struct whose size is read) changes the CodeId of every root that reaches the edited
  instance, and of no root that does not.
- Asking the identities in another order, or in a fresh database at another checkout path,
  gives the same bytes.

## Related

- `docs/v2/plan/design/item-hash.md`: the item-level code and interface hashes this builds on.
- `docs/v2/plan/design/caching.md`: what stored code is keyed by.
- `docs/v2/plan/design/comptime.md`: `fn_info(f).code_id`, the one user-facing door.
- `docs/v2/plan/design/verify.md`: the edit fuzzer.
- `docs/v2/plan/design/kernel.md`: the kernel's step identity.
- `docs/v2/plan/design/polymorph.md`: the share key and shapes.
- `docs/v2/plan.md`: pure scripts and trusted doors.
