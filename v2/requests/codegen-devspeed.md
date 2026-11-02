# Requests from codegen-devspeed

What the dev-speed fixes in `crates/hir-ty-codegen/` (mod.rs, fragment.rs, tier.rs, cached.rs,
backend.rs, new copies.rs) need from paths other lanes own. Report:
`/tmp/blaze-lanes/v2-codegen-devspeed.md`.

## codegen-adapter (`v2/`, `crates/hir-ty-mir/`)

1. **Call `settle`, not `inline` + `promote`.** `mod.rs`'s new `settle(ctx, cg)` is what every IR
   build goes through before a backend: `inline`, `promote`, then `copies::lower_small` (small
   `memcpy`/`memmove` of a known size become loads and stores, and every remaining libcall is
   called in place), then `promote` again. `compile_as`, the tier body and the inlinable bodies
   call it; `v2::backend`'s `prepare` should too, so v2's IR is v1's.
2. **`v2::build` is read.** `Recorder::new` (fragment.rs) sets `Recorder::unit` from
   `v2::build::building()`. With `Some`: `build` takes `aborts` and the compiling crate's
   `debug_assertions` from it (what `ub_checks()` and core's `check_*_ub()` become, by rustc's
   rule in `ub_checks_in`), and `inlined_callee` and `show_callee` build nothing in.
3. **Keep storage markers at MIR level 1 when the code generator uses them**
   (`hir-ty-mir/src/mir/optimize.rs:110`). rustc runs `RemoveStorageMarkers` only when it
   emits no lifetime markers (`!sess.emit_lifetime_markers()`, which is at `-Copt-level=0`).
   blaze's code generator always uses them: `overlap.rs` shares the slots of borrowed locals
   from them, as LLVM's stack colouring does from lifetime markers. Codegen now asks for level
   1 for every non-library crate at dev (`mod.rs` `mir_opt_level_of`). Without markers, every
   borrowed local is live through the whole body (`mir/storage.rs`), and the frames of
   functions with many borrowed locals grow back toward #1307's sizes. Suggested change: a
   parameter of `optimize` (or a level variant) for "the code generator emits lifetime
   markers", which codegen passes as true.
4. **Fix `optimize.rs`'s module doc**: "the code generator asks only when told to
   (`BLAZE_MIR_OPT`)" is no longer true. Codegen uses rustc's level (1 for a crate at -O0, 2
   for library crates and above -O0), and `BLAZE_MIR_OPT` only overrides it.
5. **rustc's debug-build UB checks** (`CheckAlignment`, `CheckNull`, `CheckEnums`,
   `passes_not_ported`) are a behavior difference: rustc's debug build panics on a misaligned
   or null raw-pointer dereference and on an invalid enum transmute; blaze's doesn't. They are
   on when `ub_checks()` is (`sess.ub_checks()`). Now that codegen has that value
   (`FunctionCx::ub_checks`), they can be ported and keyed on it.
6. **v2 and inlining.** With a v2 unit building nothing in, the largest dev-speed fix (every
   dependency's `#[inline(always)]` body built into its callers, as LLVM's always-inliner does
   at -O0) is lost on the v2 path. `Recorder::built_in` now lists every instance whose body a
   function has in it, transitively (`Inlinable::built_in`). Filling
   `blaze_codegen::CompiledFunction::inlined` with their code hashes, and adding their requests
   to `refs::walk`, would let v2 build them in too.
7. **wasm under v2**: `inlined_callee` returns `None` for a v2 unit before its wasm branch, so
   wasm-bindgen's descriptor shims (which must have their marker import's call built in, see
   `always_inlined`) would not work under v2 until item 6 is done.

> forwarded to codegen: items 1 to 7 are in lane codegen's own paths (`crates/hir-ty-codegen/`, `crates/hir-ty-mir/`, `crates/blaze-codegen/`), for codegen-adapter.

## codegen (parent) / plan

8. **Std at its shipped level.** cargo's debug ripgrep links std's non-generic code as shipped
   (`-O3`); blaze builds the std unit with the program's backend and level
   (`blaze-engine/src/lib.rs` `std_unit`, `..args.clone()`), which at dev is Cranelift with
   `opt_level=none` (`hir-ty-codegen/target.rs:324`). v2's plan says std non-generic compiles
   as shipped: the std unit's `Spec` should be LLVM at -O3 (or the release backend), not the
   dev profile's.
9. **Docs** that this change makes untrue: `docs/support.md` "MIR optimizations" ("0, the
   default, runs none") and `docs/mir-passes.md` ("blaze picks the level from `BLAZE_MIR_OPT`
   (0, the default, 1 or 2)"). The default is now rustc's level per crate, and `BLAZE_MIR_OPT`
   overrides it for every crate.
10. **v1's store key** (`cached.rs` `Entry::key`) names an instance without its crate's
    `debug-assertions`, which `ub_checks()` now follows. I bumped the entry version to 13 so no
    entry made under the old always-false rule is read. Entries are still told apart only by
    ` checked` (overflow checks), which matches `debug-assertions` in cargo's default
    profiles but not in a profile that sets one without the other. v2 keys on
    `Spec::debug_assertions`, so the gap is v1-only.

8b. **plan: tests are built at the profile's opt-level** (lane file, #1404/#635). A `cargo
    test` build is the `test` profile, which inherits `dev`: overflow checks and debug
    assertions on, so `ub_checks()` is on as well (`Spec::debug_assertions`). A `--release`
    test build is the `bench` profile, which inherits `release`. The policy should give test
    units the profile's `Spec` (backend, `overflow_checks`, `debug_assertions`), not a fixed
    one, so a test runs the code its profile would build.

> forwarded to codegen: items 8, 8b and 10 (codegen's `blaze-engine`, policy and v1 store key).
> forwarded to coordinator: item 9, `docs/support.md` and `docs/mir-passes.md`, are the coordinator's docs, not among the paths `requests` applies.

## Past rustc's -O0 (the "everything is ours" rule, docs/v2/plan.md)

rustc's debug build is the floor, not the target. These keep Rust's semantics and make dev code
faster than cargo debug.

11. **plan (parent): tier code that no edit reaches (#807).** Dependencies from the registry and
    git, and std, are never edited in a session, and their units are cached by key.
    `Spec::backend` is already per unit (`blaze_codegen::Backend::Llvm { opt }`). As the lane
    file's #807 says, Cranelift comes first and LLVM runs in the background, not `-O2` on
    every dependency function: a dependency unit is built with Cranelift, and where it is hot,
    or always for std (`opt: 3`, as shipped), LLVM's build of the same key replaces it once it
    is done. ripgrep's hot loops are all in dependencies (aho-corasick, memchr,
    regex-automata), so once tiered, dev ripgrep should approach cargo release. Not allowed:
    an LLVM failure that falls back to Cranelift (no-fallback; `backend.rs`'s `produce` has
    one today). A failed LLVM build is an error for the unit's tier, and the Cranelift build
    stays as the tier below it, not as a substitute.
12. **adapter: the MIR level follows the unit.** `mod.rs` `mir_opt_level_of` now picks rustc's
    level from the crate and from the process's backend (`backend::above_o0`). Under v2 a
    unit's backend is its `Spec`'s, so `v2::build::Build` should carry the unit's MIR level
    (2 for `Llvm { opt > 0 }` and library crates, else 1). `mir_opt_level_of` would then read it
    as it reads `debug_assertions` (`Recorder::unit`). The refs walk and `Hashes::code` must
    read the same body as the IR builder (`compiled_mir_body`), or a unit's references and key
    would be of a different body than its code.
13. **adapter (hir-ty-mir): port rustc's MIR `Inline` pass** (listed in `passes_not_ported`
    for level 2). rustc ships std's MIR with it run, so every std `#[inline]` helper arrives
    already inside its caller. blaze approximates that with `inlinable`'s size limits
    (`mod.rs` `inlinable`: at most 4 blocks, 12 statements and 64 instructions), which leave
    out most `assert_unsafe_precondition!` wrappers (`ptr::copy_nonoverlapping`,
    `slice::from_raw_parts`) now that `ub_checks()` is on at dev. Running the ported pass on
    library crates at level 2, and on dependency units under item 11, would let codegen
    drop its own size heuristic for them. Code hashes of the inlined callees go into the
    caller's key (`CompiledFunction::inlined`), as in item 6.
14. **parent: a Cranelift backend that optimizes.** `blaze_codegen::Backend::Cranelift` has no
    level, and `hir-ty-codegen/target.rs:324` sets `opt_level=none`. Cranelift's `speed`
    (egraph rewrites, GVN, LICM, redundant-load removal; production in wasmtime) is a cheaper
    middle step than LLVM for item 11's units, and possibly for workspace code. Proposed:
    `Backend::Cranelift { speed: bool }`. `target.rs` would then keep one ISA per level (as
    it already keeps `target_for_large`). I'll make the `target.rs` side once the variant
    exists.

> forwarded to codegen: items 11 to 14 (`Backend::Cranelift { speed }` is codegen's own interface in `crates/blaze-codegen/`; the MIR `Inline` pass is in `crates/hir-ty-mir/`).
