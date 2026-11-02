---
status: plan
date: 2026-10-30
summary: lane no-fallback, front area (hir-def, hir-expand, hir, hir-ty, hir-ty-mir, hir-ty-codegen, hir-ty-matchck, rustc_abi, mbe): every fallback site classified as error, rule or dead, with what it becomes.
---
# no-fallback: front

Part of [no-fallback](../no-fallback.md). Kinds: **error** (a real fallback, now a typed error the
build stops on), **rule** (rustc or the platform does the same; it stays, with the rustc function
it matches), **dead** (nothing reaches it, or v1's rustc-beside-blaze code; deleted).

The front end's crates are split three ways, each with its own table:

| crates | table |
|---|---|
| hir-def, hir-expand, mbe | [front/def.md](front/def.md) |
| hir-ty, hir-ty-matchck, rustc_abi | [front/ty.md](front/ty.md) |
| hir-ty-mir, hir | [front/mir.md](front/mir.md) |
| hir-ty-codegen | below |

## hir-ty-codegen

Line numbers are of the tree after this lane's edits (2026-10-30).

| site | kind | what it did | what it is now |
|---|---|---|---|
| `backend.rs:220` `Backend::produce`, `Counts::fallbacks`, `Tally::fell_back`, the `wasted` word | error | with `BLAZE_BACKEND=llvm`, a function LLVM had no lowering for (`llvm::Unhandled`) was compiled by Cranelift instead, its reason counted in a map nothing read any more | `CodegenError::LlvmRefused(LlvmUnhandled)`, the reason typed (`Opcode`, `Type`, `Shape`, `Relocation`, `Llvm`); the driver wraps it with the unit and item. The counters are deleted. Test: `llvm.rs` `a_function_llvm_cannot_lower_is_refused` |
| `tier.rs:1110` `produce` | error | a tier's LLVM refusal became `Unsupported(String)` | `CodegenError::LlvmRefused`, the same typed reason |
| `backend.rs:212` `LLVM_LIMIT` | rule (policy) | in an edit's build (not a whole program), a body of more than 10 000 Cranelift instructions goes to Cranelift | stays: decided by the body alone before either producer runs, never by a failure, and documented with its measurement. No longer counted as a fallback |
| `backend.rs:218` `plain_handlers` | rule (policy) | in an edit's build, a body with several handlers on a call goes to Cranelift (LLVM's landing pads cost 17 ms each) | stays, as above |
| `llvm.rs:214` `super::LARGE` | rule (policy) | a body of more than 50 000 instructions is compiled by LLVM at `-O0` | stays: deterministic by size, documented (dx's minutes in LLVM's register allocator), and counted in `Tally::large` |
| `llvm.rs:1705` `Unhandled::Kept` | dead | "what an earlier build of the same IR was told, as the store kept it": a cached refusal; nothing made it | deleted |
| `mod.rs:171` `CodegenError::Unsupported` doc "fall back to rustc" | error (doc) | nothing matches on `Unsupported` to take another path; the doc was v1's | doc says the build stops on it. Its 286 `unsupported!` sites are stops, not fallbacks. Their reasons are still strings |
| `mod.rs` `CodegenError::NoSymbol`, `Private`, `no_symbol()` | dead | v1's "rustc has no symbol for the function" variants; nothing made or read them | deleted |
| `mod.rs` `Phases` (`fallback`: "functions left to rustc") | dead | v1's per-round timings; no user | deleted |
| `mod.rs` `async_blocks`, `held_blocks`, `infers_a_future` | dead | v1's "every function that touches a future has to come from the same compiler" (rustc lays futures out differently); no caller | deleted |
| `mod.rs:1328` `mir_opt_level` | error | `BLAZE_MIR_OPT` set to a value that is not 0, 1 or 2 printed a line and used rustc's level | `CodegenError::Backend` naming the value; `mir_opt_level_of`, `mir_options_of`, `body_options` return `Result` and pass it up. Test: `a_mir_opt_level_that_is_none_is_an_error` |
| `v2/frontend.rs:382` `shared_args` | error | any `hir_ty_share::Unshared` reason (a missing layout, a constant that failed to evaluate, an intrinsic missing from the table, a place the key can't read) quietly compiled the generic per instance | only `Recursive` (salsa's cycle result), `Shim` and `PerInstance` (per instance by what they are) build per instance (`per_instance`). Every other reason stops the build with the instance and the reason. Test: `only_what_is_per_instance_is_built_per_instance` |
| `llvm.rs:304`, `:330` bodies shown to LLVM that fail to lower are unshown | rule (optimization) | a callee's body offered to LLVM for inlining that LLVM can't lower is left a declaration, called | stays: the program is the same, and the callee's own compile reports the same `Unhandled` as an error |
| `imports.rs:245-294` `export` returns `None` | rule (optimization) | a callee whose body can't be exported for import isn't built in | stays, for the same reason: inlining is an optimization, never a different program |
| `tier.rs:783` `build_body`, `:588` `refused` | rule (optimization) | a body that can't be built in at a tier is called instead, said with `BLAZE_DUMP_TIER` | stays, as above |
| `fma.rs`, `f16_f128.rs`, `sve.rs` | rule | each lowers as LLVM does for rustc, bit for bit, and refuses (`Unsupported`) what it can't match | stay: no other path is taken |
| `fncache.rs:2012` `cache_code` | rule (extension) | `#[blaze::cache]`'s `__blaze_cache_code` is answered with hash 0 and the reason a function can't be cached | stays in codegen: the program is given the reason as the extension defines. Whether the `blaze` crate then runs the function uncached is the tools area's |
| `places.rs:248`, `mod.rs:1711` source places of macro code | rule | an expression with no syntax (desugared) is placed at the macro's own place in debug info | stays: debug lines only, no program semantics |
| `mod.rs:1593` `declared_method` `unwrap_or(func)` | rule | an impl method with no trait method of its name is itself | stays: only an invalid program (E0407) reaches it, and the front end reports that |
| `v2/places.rs:125` "the intrinsic `{name}`, which has no fallback body" | rule | a call of an intrinsic with neither a lowering nor a fallback body (rustc's `#[rustc_intrinsic]` default body) is an error | stays: the word is rustc's |
| `v2/places.rs:125` "the intrinsic `{name}`, which has no fallback body" | rule | a call of an intrinsic with neither a lowering nor a fallback body (rustc's `#[rustc_intrinsic]` default body) is an error | stays: the word is rustc's |
| `asm.rs:1567` `Assembler::Cc` | rule | `BLAZE_ASSEMBLER=cc` assembles with a C compiler, else LLVM; none found is an error | stays: chosen by the environment, never by a failure |
