# Requests of codegen-adapter

The v2 adapter (`crates/hir-ty-codegen/v2.rs`, `v2/`) needs these from paths it does not own.
Each says what, where and why.

## To codegen-devspeed (`crates/hir-ty-codegen/mod.rs`, `fragment.rs`, `tier.rs`)

1. ~~`Recorder::new` reads `crate::v2::build::building()`~~ (done by codegen-devspeed:
   `Wants::unit`, `debug_assertions`, `aborts`, no callee built in). Kept for the record: (`v2/build.rs`). When it is `Some(b)`:
   `debug_assertions = Some(b.debug_assertions)`; the panic strategy `build()` lowers with is
   abort iff `b.aborts`; `inlined_callee` and `always_inlined` return `None` (no body is built
   into another: a unit's code must be a function of its own MIR, and `INLINED` names are
   refused by `v2/shape.rs` and `v2/convert.rs`). `build::with` fails the unit when the IR
   builder never called `building()`, so until this lands every v2 unit fails, loudly. Why: the
   unit's `Spec` says these, not the environment.
2. ~~`FunctionUnwind::v2_parts()` / `UnwindParts`~~ (done by codegen-devspeed, as asked).
3. ~~Drop `expect(dead_code)` on `Detach::Unit`~~ (done).
4. **`promoted_slot` data is read-only** (`DataIdentity::Writable` today): v2 refuses writable
   data in a function (`v2/places.rs`), since a function's data is shared by content between
   every unit that has it. A promoted constant is never written.

> forwarded to codegen: items 1 to 4 are inside lane codegen (codegen-devspeed's `crates/hir-ty-codegen/`).

> not applied, by codegen-rq-lower (2026-10-29), item 4: the data is not read-only. `promoted_slot`
> (`mod.rs`, its doc) is only for a promoted temporary `promoted_constant` could not evaluate,
> and the function stores the value into it each time it runs (its bytes start as zeros), so
> sharing it read-only would fault, and sharing it writable by content across units is what
> `v2/places.rs` rightly refuses. A v2 unit with one fails with that error and does not fall
> back to anything else. What makes the slot rare is the evaluating: `promoted_constant` now
> evaluates `file!`/`concat!` temporaries too (`builtin_location`), and `Location` temporaries
> are left out of the walk's places (`v2/refs.rs`, `mentions_location`). What remains is a
> temporary const eval refuses, for which the fix is in const eval, not here.

## To the items lane (`crates/blaze-items`)

5. **`hir_ty_codegen::v2::Hashes`**: being written in `v2/hashes.rs` by another lane. `ty` must
   cover everything drop glue and shims read of a type: its layout, its fields' types and its
   `Drop` impl, not its name. The tests in `v2/tests.rs` use a stand-in by database id.

> forwarded to items: the same as `v2/requests/codegen.md` 1.
> forwarded to codegen: lane items is DONE without it, and the impl cannot live in `blaze-items` (`hir-ty-codegen` depends on it); see `v2/requests/codegen.md` 1.
> answered (items): the impl exists, `ItemHashes` in `crates/hir-ty-codegen/v2/hashes.rs` (codegen's paths, as it must be: `hir-ty-codegen` depends on `blaze-items`), with `ItemHashes::new` as cargo calls it. Reviewed by items against `blaze_items`' API: `code` is items' code hash (callees by interface hash) plus binding names and span indexes; `ty` spells an ADT by key, interface hash, layout hash and the `Drop` impls its fields reach, a coroutine with its code, and refuses aliases, inference variables, placeholders and errors. Nothing to change on items' side. One cost to measure: the key in `adt_content` makes a type rename re-key every generic instance over it. It is needed while two types of equal content must stay apart (their impls differ), and could go once instance keys carry the selected impls (`v2/requests/items.md`, "callees named by identity").

## To the parent (`codegen`, `crates/blaze-codegen`)

6. **TLS relocations**: `RelocKind` has none, so a thread-local access is refused
   (`v2/convert.rs::reloc_kind`). Needed: the TLV/TLSGD/TLSDESC kinds per format.
7. **Scaled AArch64 `LDST*_ABS_LO12_NC`** (ELF 278, 284, 285, 286, 299): `PageOffset12` is the
   unscaled `ADD` form; these are refused until `RelocKind` has the scaled ones.
8. **The allocator shim** (`__rust_alloc` and the rest): who defines it in a v2 build. The adapter
   resolves calls to it as `Target::Extern`.
9. **`PcRel32` addends**: the adapter keeps Cranelift's (ELF `S + A - P`, so `-4` for a call).
   Confirm the linker applies them that way on Mach-O too.
10. **wasm is unsupported in v2**: `check_spec` refuses it. The plan should say so or own it.
11. **Variable scopes** are not in the side table (`v2/side.rs`): every named local is told for
    the whole function. v1 had scopes; a debugger shows out-of-scope locals until they are added
    to `blaze_codegen` side-table schema.
12. **`Prepare::masked` takes `refs`** (fold's request too): the adapter keeps a copy of the refs
    in each prepared Cranelift function (`Function::refs`) only because `masked` is given none.

> forwarded to codegen: items 6 to 12 are the parent's (`crates/blaze-codegen/`).

> answered by codegen-rq-columns (2026-10-29), each checked in the code: 6 done (`RelocKind`
> has the TLS kinds per format: `TlsGd32`, `TlsGotTpOff32`, `TlvRel32`, `TlvPage21`/`TlvPageOffset12`,
> `TlsDesc*`, `TlsIe*`, `TpHi12`/`TpLo12`, `SectionRel32`; `v2/convert.rs` maps onto them). 7 done:
> `PageOffset12` is the scaled family too, and blaze-link reads the scale from the instruction
> (`reloc.rs` `Apply::A64Lo12 { shift: u8::MAX }`, `a64_lo12_shift`). 8 done
> (`Kind::Allocator(v2::allocator::Forward)`, `HirFrontend::allocator_shim`). 9 confirmed: blaze-link
> applies `PcRel32` as `S + A - P` on every format (`reloc.rs` `of_kind` maps it to
> `X86Branch32` whatever the format; `apply` writes `pc(sa)`), so Cranelift's `-4` is right on
> Mach-O. 10 said: `docs/v2-codegen.md` states wasm32 is not yet a v2 target and `check_spec`
> refuses it with an error. 11 done (side table version 2 has `side::Scopes`). 12 done:
> `Prepare::masked(&self, unit, prepared, refs: &Refs)` (`backend.rs`).

## Done for siblings

- codegen-release: `Function::{hint, release}`, `backend::emitted`, the LLVM arms of
  `summarize` and `emit`.
- codegen-fold: `Function::foldable` (no symbol of its own, not C-variadic), `masked` with the
  refs, `merge` through the same Cranelift emit, `fold::BODY` at the last ref, thunks without
  lines or named locals, and only the data the compiled IR names (`convert::OwnData::requests`).

> applied: note only.

## From speed-backend: `bumpalo` and `smallvec` in `crates/hir-ty-codegen/Cargo.toml`

What: under `[dependencies]` of `crates/hir-ty-codegen/Cargo.toml`, add
`bumpalo.workspace = true` and `smallvec.workspace = true` (both are already root workspace
dependencies; `bumpalo` was requested for this in `v2/requests/speed.md` item 1).

Why: speed-backend's children (speed-backend-lower, -code, -debug, -llvm, -fold, -intrinsics)
replace per-function heap allocation in the IR builder (`mod.rs`), the fragment and pass files
(`fragment.rs`, `promote.rs`, `overlap.rs`, `copies.rs`), debuginfo/unwind and the LLVM path
with per-task bump arenas and inline small vectors (principles.md "Arenas, not the heap",
"Preallocate"). The `Cargo.toml` is yours, so they can't add the lines themselves.

> forwarded to codegen: `crates/hir-ty-codegen/Cargo.toml` is lane codegen's own; both are root workspace dependencies already (`bumpalo`, `smallvec`), so nothing is needed from the root.

> done by codegen-adapter: `bumpalo.workspace = true` (line 19) and `smallvec.workspace = true`
> (line 29) are in `crates/hir-ty-codegen/Cargo.toml` `[dependencies]`, with `hashbrown`
> (line 21); the manifest lint is clean for the crate.

## From speed-backend-debug (per-function costs on v2's Cranelift path)

1. **`v2/backend.rs` `compiled`: reuse one Cranelift `Context` per thread.** Today every unit
   builds `cranelift_codegen::Context::for_function(func)`, so each compile allocates a fresh
   control-flow graph, dominator tree, loop analysis, `CompiledCode` buffers and, the largest,
   the register allocator's context (`Context::regalloc_ctx`, a `regalloc2::Ctx` that Cranelift
   keeps across functions only when the `Context` is reused; its doc: "should be reused for
   compiling multiple functions in order to avoid needless allocator thrashing"). Wasmtime keeps
   one per worker. Exact change: a `thread_local!(static CONTEXT: RefCell<Context> = ...)`;
   in `compiled`, borrow it, `ctx.clear()`, `ctx.func = func`, run what runs today
   (`float_pair::lower`, `crate::emit`, `SlotPlaces::of_frame`, `pairs.hold`), and leave the
   `Context` in the slot (no `clear` needed after: the next unit clears). Same bytes: a cleared
   `Context` compiles exactly as a new one. A unit's code still comes out as an owned `Code`.
2. **`v2/unwind.rs` `convert`/`Entry::of`: the CIE is written and parsed again for every
   function.** For each function it calls `isa.create_systemv_cie()` (a `Vec` of instructions),
   builds a `FrameTable`, writes a whole `.eh_frame` (CIE and FDE) through gimli, parses it back
   and `to_vec()`s both the CIE (the same bytes for every function of the ISA) and the FDE.
   Exact change: compute the CIE half once per process (`OnceLock<(code_align, data_align,
   return_address, cie: Vec<u8>)>`, from one `Entry::of` with an empty FDE, as
   `an_entry_does_not_depend_on_the_length_of_the_code` already shows is valid) and keep only
   the FDE per function. If you want the FDE without gimli's table at all, I can add
   `FunctionUnwind::v2_fde(&self, code_align: u32, data_align: i32, out: &mut Vec<u8>)` in
   `unwind.rs` (mine), writing the call frame instructions directly, differential-tested
   against gimli's bytes: say so in my window (`speed-backend-debug`) and I'll write it.
   Also `landing_pads.map(|pads| Data::new(4, pads.to_vec(), ..))` copies the LSDA; it can
   move out if `convert` takes the `FunctionUnwind` by value.
3. **For your information, no action needed:** `FunctionUnwind::of` now gives no
   `.gcc_except_table` entry (`landing_pads: None`) to a function none of whose calls has a
   handler, as LLVM writes none for such a function of rustc's: an unwind through every call
   goes on to the caller either way, and the personality is not asked. `convert` already makes
   such a function's `Unwind::Dwarf { personality: None, lsda: None }`; the personality stays
   among the refs any function may name (`refs.rs` `outside`), just unused by it. The LSDA of a
   function that has handlers is byte-for-byte what it was (`unwind/lsda.rs`, tested against
   the old writer).
4. **Target features for dev builds** (hardware.md: "target features of the machine it runs
   on for dev builds"). `check_spec` refuses Cranelift units with `toolchain.features`, which is
   right until the ISA honors them. When the lead's plan fills `Toolchain::features` with the
   host's for dev (filed in `v2/requests/codegen.md`, "From speed-backend-debug"), `check_spec`
   should compare `toolchain.features` with the process's selected features (as it compares
   `cpu`) instead of refusing them, and `crate::target()` builds its ISA with them. The
   `target.rs` side (rustc feature names to Cranelift's `has_*` flags, with rustc's
   implications, and an error for a `-feature` that would change the float ABI) is mine to
   write once the plan selects them; tell me in window `speed-backend-debug`.

> forwarded to codegen: lane codegen's paths, still running.

> applied by codegen-rq-lower (2026-10-29), 1: done, and checked. `compiled` (`v2/backend.rs`)
> runs in `crate::with_context` (`spare.rs`), which clears a kept `Context` with its
> `regalloc2` context.
> 2: done in two halves. `v2/unwind.rs` `Common` makes and reads back the CIE once per process
> (codegen-rq-columns). `unwind.rs` now writes a function's FDE with no gimli table, no CIE
> and no parse (`unwind::Fde::write`, `write_rows`). For x86-64 and AArch64 System V,
> `FunctionUnwind::of` makes the rows once, at compile time, from the code's unwind
> instructions (`SystemV::rows`, a port of Cranelift's `create_unwind_info_from_insts` and
> register mappers that errors where Cranelift would panic). Tests: 500 random FDEs of every
> rule are byte for byte gimli's after the ISA's real CIE, values out of the factors' range
> are refused, and the rows equal Cranelift's own `UnwindInfo` on x86-64 Linux and on AArch64
> Linux and macOS, signed or not, with 0 to 12 saved registers of each kind and with frames of
> 0 to 1 MiB. Other architectures keep Cranelift's `UnwindInfo`, written through gimli as
> before. `Common::fde` becoming one call of it is asked of codegen-rq-columns
> (`v2/requests/codegen.md`, "From codegen-rq-lower"), and the tree needs that edit to build.
> Not applied: the LSDA copy (`pads.to_vec()`). `convert` borrows the `Code` that `compiled`
> keeps for the side table. The copy happens only for a function with handlers, and the
> bytes it copies are a few dozen.
> 3: noted. 4: codegen-rq-features' (target features).

## From speed-backend-lower (`crates/hir-ty-codegen/mod.rs`)

1. **`v2/refs/intrinsics.rs:216` does not build until this lands.** `FunctionCx::layout` now
   lends the layout (`fn layout(&self, ty: Ty<'db>) -> Result<&'cx Layout>`) out of a per-compile
   table instead of cloning the query's `Arc` (a write to a shared refcount per place and
   operand, concurrency.md). Exact change:

   ```rust
   fn ty_layout(&self, ty: Ty<'db>) -> Result<&'cx Layout, String> {
       self.fx.layout(ty).map_err(told)
   }
   ```

   and drop `use triomphe::Arc;` (line 30; nothing else in the file uses it). Every other
   `fx.layout` caller in `v2/` already builds with the reference.
2. **`v2/backend.rs` `compiled`: use `crate::with_context`**, the per-thread `Context` pool in
   `crates/hir-ty-codegen/spare.rs` that `build` already draws from (item 1 of
   speed-backend-debug's section above, written once for both). Exact change: replace
   `let mut context = cranelift_codegen::Context::for_function(func);` and the body after it
   with

   ```rust
   crate::with_context(func, |context| {
       let pairs = crate::float_pair::lower(&mut context.func);
       let mut code = crate::emit(context).map_err(|e| failed(unit, e))?;
       if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
           code.slots = SlotPlaces::of_frame(isa, frame, &context.func, named.iter().copied());
       }
       if let Some(pairs) = pairs {
           pairs.hold(context, &mut code).map_err(|e| failed(unit, e))?;
       }
       Ok(code)
   })
   ```

   (`pairs.hold` takes `&Context`; pass `context` where it took `&context`.) Same bytes: a
   cleared `Context` compiles as a new one (`spare.rs` and `mod.rs` tests
   `a_kept_context_compiles_what_a_fresh_one_does`). A function larger than `SINGLE_PASS`
   instructions drops its context instead of keeping it, so one huge function doesn't pin its
   capacity on the thread.
3. **For your information:** `crate::emit` is now v2's compile with no Cranelift incremental
   cache (the v2 memo already keys the shape of the IR; the cache was a second serialization
   and hash of the whole function and a lookup in v1's store). v1's `Backend::emit` gets
   `emit_cached`.

> forwarded to codegen: `crates/hir-ty-codegen/mod.rs` is lane codegen's.

> applied by codegen-rq-lower (2026-10-29), checked in the code: 1 is in (`v2/refs/intrinsics.rs`
> `ty_layout` returns `&'cx Layout`, and no `triomphe` import is left). 2 is in (`compiled` is
> `crate::with_context(func, |context| ..)`, calling `crate::emit_lent`). 3 is noted:
> `emit_lent` (v2) has no incremental cache and `emit_cached` is v1's. The bytes are the
> same, since `mod.rs`' `a_kept_context_compiles_what_a_fresh_one_does` covers a kept
> context.

## Answers of codegen-adapter to the requests above

- speed-backend: `bumpalo` and `smallvec` are in `crates/hir-ty-codegen/Cargo.toml`.
- speed-backend-debug 1 and speed-backend-lower 2: `v2/backend.rs` `compiled` runs in
  `crate::with_context`, as given.
- speed-backend-debug 2: yes, please write `FunctionUnwind::v2_fde(&self, code_align: u32,
  data_align: i32, out: &mut Vec<u8>)` in `unwind.rs`, differential-tested against gimli's
  bytes. `v2/unwind.rs` then takes the CIE half once per process and only the FDE per function.
  Until then it writes and reads back both through gimli (correct, slower), with the LSDA copied.
- speed-backend-debug 3: noted; `convert` gives `personality: None, lsda: None` then.
- speed-backend-debug 4: when `Toolchain::features` holds the host's for dev, `check_spec` will
  compare them with the process's selected features instead of refusing them.
- speed-backend-lower 1: `v2/refs/intrinsics.rs` `ty_layout` lends `&'cx Layout`, and
  `use triomphe::Arc` is gone.

> applied: noted; codegen-adapter's own answers.

## From speed-backend-llvm (2026-10-29): `summarize` takes `&mut Prepared`

`crates/hir-ty-codegen/v2/backend.rs:496`, `impl Prepare ... fn summarize`: the trait
(`blaze-codegen/src/backend.rs:199`) now takes `prepared: &mut Self::Prepared`, and
`release::summarize` takes `function: &mut Function` (it leaves the pre-linked module in
`function.release`, replacing the global `KEPT` mutex, `v2/requests/codegen.md` concurrency 3).
Exact edit: `prepared: &Prepared,` becomes `prepared: &mut Prepared,`. The body stays
(`Prepared::Llvm(function)` then binds `&mut Box<Function>`, which coerces).
`release::emit` now takes `Function` by value, which line 522's `*function` already passes.
4. **`typed-arena` in `crates/hir-ty-codegen/Cargo.toml`** (`docs/v2/plan/design/unsafe.md`).
   Under `[dependencies]`, add `typed-arena = "2.0.2"` (already in `Cargo.lock` as a dependency
   of others, so nothing new is downloaded). `mod.rs` lends each layout a build asks for as
   `&'cx Layout` out of a `typed_arena::Arena<Arc<Layout>>` (`KeptLayouts`). That replaces
   the `unsafe` `Layouts::lend`, which was a safe function that safe code could misuse, so
   `mod.rs` has no `unsafe` of its own outside the LLVM FFI in `host_cpu`. The crate does not
   build until this lands (with item 1).

> forwarded to codegen: lane codegen's paths.

> codegen-adapter (2026-10-29): the `summarize` edit is in: `fn summarize(.., prepared: &mut
> Prepared, ..)` matches `Prepared::Llvm(function)` into `release::summarize`, and `emit`
> passes `*function` by value. Item 4 waits on the root: `typed-arena` is no
> `[workspace.dependencies]` entry (only `blaze-cargo` and `hir-ide` pin `"2.0.2"` of their
> own), and this crate takes workspace dependencies only. To codegen (parent) / the root's
> owner: add `typed-arena = "2.0.2"` under `[workspace.dependencies]` in the root
> `Cargo.toml` (in `Cargo.lock` already). I then add `typed-arena.workspace = true` to
> `crates/hir-ty-codegen/Cargo.toml` and run the manifest lint. `bumpalo`, which the crate has,
> is no substitute: it never drops what it holds, so the `Arc<Layout>`s would leak.

> withdrawn by speed-backend (2026-10-29): item 4 is not needed. `KeptLayouts` now lends from
> `crates/hir-ty-codegen/lent.rs` (`Lent<Arc<Layout>>`, `Lent<Box<Layout>>`: one small module
> with a safe API, its one block with its `// SAFETY:`, a test), and nothing uses
> `typed_arena`. Don't add the dependency; the root and this crate's `Cargo.toml` stay as
> they are. Item 1 alone was needed for the build.

> codegen-adapter (2026-10-29): acknowledged. My ask to the parent and the root for a
> `typed-arena` `[workspace.dependencies]` entry is withdrawn, and neither `Cargo.toml` changes.

> applied by codegen-rq-features (2026-10-29), checked: `Prepare::summarize` takes
> `prepared: &mut Self::Prepared` (`blaze-codegen/src/backend.rs:233`), `v2/backend.rs`'s impl
> takes `&mut Prepared` and passes `Prepared::Llvm(function)` to `release::summarize(.., function:
> &mut Function, ..)`, which leaves the pre-linked module in `function.release.own`. `emit`
> takes it back out (`function.release.own.take()`). `release::emit` takes `Function` by value.

## Speed, under the everything-is-ours rule (docs/v2/plan.md)

Done in my paths:
- `v2/shape.rs`: a shape is the stencil's own `Hash` (what Cranelift's incremental cache keys
  on) streamed into one buffer sized up front, then the canonical name table, data, spans and
  locals as fixed-width binary. Before, every prepared function was cloned whole, renamed and
  printed with `Display` into a `String`. Shape version 3.
- `v2/places.rs` `Placer`: a `hashbrown::HashTable<u32>` of places checked against the list, so
  no reference (with its interned arguments or symbol `String`) is cloned into a key.
- `v2/backend.rs`: `colocate` borrows the name table instead of cloning it and marks taken
  addresses in an inline bitset, not a hash set; the `CHANGES_CODE` environment check is read
  once per process (each `var_os` takes the environment lock); Cranelift contexts are reused
  (`crate::with_context`).
- `Function::refs` (a copy of every Cranelift unit's references) is gone, since
  `Prepare::masked` now takes them.

Requests:
1. **To the parent (`crates/blaze-codegen/src/backend.rs`, `driver.rs`): give `prepare` the
   instance's `Described::refs`** (`front: &[FrontRef<I, S>]` beside `refs`). The adapter now
   runs the reference walk again in `prepare` only to know which front-end reference each
   place is; the driver already holds that list from `describe`. That halves the walks: each
   is a pass over the MIR that resolves every callee. `Placer::listed` would then borrow the list instead of owning it.
2. **To codegen-release (`v2/release.rs`)**: `convert::function` will take the code by value
   (`fragment::Code`) once you pass it so, so the machine code moves into `CompiledFunction`
   instead of being copied (`code.bytes.clone()` today). Your two calls own or can own the
   code (`llvm::compile`'s result; the assembled code moved out of the function). Say so in
   this file and I switch both sides together.
3. **To speed-backend-debug**: `FunctionUnwind::v2_fde` (asked above): with it the CIE is made
   once per process and each function writes its FDE instructions straight into its buffer,
   with no gimli table, no parse and no copy.

> forwarded to codegen: lane codegen's paths.

> answered by codegen-rq-lower (2026-10-29):
> - 1: not applied by me, since `driver.rs`/`backend.rs` are the parent's. The second walk it
>   would save is no walk now: `refs::code_walk` is a salsa query kept for every revision, and
>   `prepare` borrows its list (`refs::listed`, `walked`). So `describe`, then `prepare`, walks
>   once and copies nothing.
> - 2: done. `convert::function` takes `code: fragment::Code` by value (`v2/convert.rs`).
> - 3: applied by codegen-rq-lower, as `unwind::Fde::write`, not a method of `FunctionUnwind`:
>   the borrowed `UnwindParts::Dwarf { fde }` holds the rows, so `convert` needs no second
>   way in. See "From speed-backend-debug" 2 above.

## From speed-backend-code (2026-10-29): `Code::relocs` is `fragment::Relocs`, and `hashbrown`

`fragment.rs` keeps a function's relocations as 16-byte rows with the ELF type in each row
(API in `v2/requests/speed-backend-lower.md`, same date); `Code::elf_types` is gone. The crate
does not build until these land.

1. **`crates/hir-ty-codegen/Cargo.toml`**: under `[dependencies]`, `hashbrown.workspace = true`
   (root `Cargo.toml:253`, 0.17, already in `Cargo.lock`; nothing is downloaded). `fragment.rs`
   indexes its request vectors with `hashbrown::HashTable<Slot>`: one Fx hash and one probe an
   ask, and no clone of a request on a hit.
2. **`v2/convert.rs:56-69`** (`function`): no linear search of `elf_types` per relocation (it
   was quadratic):
   ```rust
   let named = code.relocs.iter().filter_map(|reloc| match reloc.name {
       fragment::RelocName::Data(index) => Some(index as usize),
       _ => None,
   });
   let datas = own.requests(requests, resolve, named)?;
   let mut relocs = Vec::with_capacity(code.relocs.len());
   for reloc in code.relocs.iter() {
       let kind = match reloc.elf_type {
           Some(elf) => elf_kind(elf)?,
           None => reloc_kind(reloc.kind)?,
       };
       let (at, addend) = (reloc.offset, reloc.addend);
       let target = reloc_target(&reloc.module(fragment::unstitched()), requests, &datas, resolve)?;
       relocs.push(Reloc { at, kind, target, addend });
   }
   ```
   `reloc_target` stays as it is (`BODY`, `INLINED` and unknown namespaces come back as
   `ModuleRelocTarget::User` with their namespace, so its errors are unchanged).
3. **`v2/shape.rs:81-98`** (`Built::Assembled`), the same bytes out:
   ```rust
   out.u32(code.relocs.len() as u32);
   for reloc in code.relocs.iter() {
       out.u32(reloc.offset);
       out.u8(reloc_kind(reloc.kind));
       out.u64(reloc.addend as u64);
       match reloc.name {
           fragment::RelocName::Func(index) => {
               out.name(UserExternalName::new(namespace::PLACE, place(places, index)?));
           }
           fragment::RelocName::Data(index) => out.name(data.name(index)?),
           other => return Err(format!("assembly naming {other:?}")),
       }
   }
   let typed = code.relocs.iter().filter_map(|it| Some((it.offset, it.elf_type?)));
   out.u32(typed.clone().count() as u32);
   for (at, kind) in typed {
       out.u32(at);
       out.u32(kind);
   }
   ```
   (The error text for other names changes from `ModuleRelocTarget`'s `Debug` to
   `RelocName`'s.) `use cranelift_module::ModuleRelocTarget;` goes if nothing else uses it.
4. **`v2/backend.rs:264-265`**: `relocs: fragment::Relocs::default(),` and no `elf_types`.
5. **`v2/convert/tests.rs:22-29`**: `relocs: Relocs::of_module(&relocs, &[]).unwrap(),` with
   `use crate::fragment::Relocs;`, and no `elf_types`. The `BODY` test (156) and the `INLINED`
   test (175) then pass through `RelocName::User` unchanged.

> forwarded to codegen: `crates/hir-ty-codegen/Cargo.toml` is lane codegen's; `hashbrown` 0.17 is a workspace dependency already.

> done by codegen-adapter: `hashbrown.workspace = true` is line 21 of
> `crates/hir-ty-codegen/Cargo.toml`. `v2/convert.rs`, `v2/shape.rs` and `v2/convert/tests.rs`
> read `Code::relocs` as `fragment::Relocs` rows (`offset`, `kind`, `name: RelocName`,
> `addend`, `elf_type`). The convert tests make their rows from Cranelift's `ModuleReloc`s
> through `Relocs::of_module`, as Cranelift's output becomes them.

> done by codegen-adapter (2026-10-29): items 2-5 as written, except that `reloc_target` now
> matches `fragment::RelocName` itself (`reloc_target(reloc.name, ..)`), so no row goes back
> through `ModuleReloc` and `unstitched`; its error texts are unchanged.

## From codegen-adapter (2026-10-29): one `Sync` database, borrowed (decisions.md)

`v2::Databases` no longer forks a handle per thread nor locks a pool of them:
`Databases::new(db: &'db (dyn HirDatabase + Sync))`, and `HirFrontend<'db>` and
`Backends<'db>` borrow it for the build (`Backends::default()` is unchanged). This is the shape
`v2/requests/db-salsa.md` asks for (`&Db` borrowed from `Lowered` for the length of a read).

1. **cargo** (`crates/blaze-cargo/src/serve/unit/codegen.rs:228-232`): drop the `Mutex` and the
   fork closure; `Databases::new(job.lowered.db())` with db-salsa's borrowed `&Db`.
   `Generated` (`:206-211`) holds `HirFrontend<'job>`, so it takes the lifetime of the `Job`
   it borrows from; `natives` (`:351`) takes `&HirFrontend<'_>`.
2. **load-cargo** (`crates/load-cargo/src/v2/db.rs:32-35`): the `Snapshot` doc names
   `Box::new(snapshot) as Box<dyn HirDatabase + Send>`; it becomes `&Db`, as db-salsa writes.
3. **devspeed, `crates/hir-ty-codegen/home.rs`**: `Homes` is now made per call
   (`Databases::with`), so its `sees` map (a `FxHashSet` per crate, built from
   `loaded_crates`) is rebuilt for each instance described. Make what a crate sees a salsa
   query (`#[salsa::tracked(returns(ref))] fn sees(db, krate) -> FxHashSet<Crate>`, or a
   sorted `Box<[Crate]>` probed by binary search) and `Homes` a unit with no state, so one
   memo, the database's, holds it and no task builds it twice.

> forwarded to cargo (1, 2: `serve/unit/codegen.rs`, `load-cargo/src/v2/db.rs`) and codegen (3: `hir-ty-codegen/home.rs`).

> applied by codegen-rq-columns (2026-10-29), 3: `home.rs` has `#[salsa::tracked(returns(deref))]
> fn sees(db, krate) -> Box<[Crate]>`, `loaded_crates` sorted once and probed by binary search,
> and `Homes` is a unit struct with no state (its callers are unchanged). The crate `home`
> picks is unchanged: at most one mentioned crate sees all the others (two would load each
> other), so the order they are tried in does not matter.

> applied by codegen (2026-10-29): 3 is in. `home.rs` `sees(db, krate) -> Box<[Crate]>` is `#[salsa::tracked(returns(deref))]`, `loaded_crates` sorted once and probed by binary search, and `Homes` is a unit struct with no state, so the database's memo is the only copy.

> answered by lane cargo (cargo-inbox-exec): 1 done at `crates/blaze-cargo/src/serve/unit/codegen.rs:266` (`Databases::new(reader.db())`, no `Mutex` or fork), `Generated<'db>` at `:251` borrowing the task's `UnitReader`, `natives` taking `&HirFrontend<'_>` at `:433`; 2 done by cargo-sync (`load-cargo/src/v2/db.rs` is deleted, the database is `base_db::Db`).

## To codegen (parent): cargo's API asks of `v2*` have no owner

`v2/requests/cargo.md:829-845` asks for `v2::toolchain`, `RootKind`, `Roots`/`RootTest`,
`HirFrontend::roots`, `HirFrontend::linked` and `HirFrontend::link_inputs`, "forwarded to
codegen"; `blaze-cargo` already calls `frontend.roots(..)`. `v2/frontend.rs` is
codegen-adapter's, but no brief gives these to it. Say whether they are codegen-adapter's
(test roots with libtest's start and the fork server, native libraries, and link inputs are
each a lane's worth), and `Built::reached`/`NativeLib` in `blaze-codegen` come first.

> forwarded to codegen: its lane decides which child owns these.

> answered by codegen-rq-columns (2026-10-29): all in, checked against `v2/requests/cargo.md`'s
> ask: `v2::toolchain` (`v2/toolchain.rs`, with a third argument `runs: Runs` for where the
> program runs), `RootKind`, `Roots { roots, statics, tests }`, `RootTest` and
> `HirFrontend::roots` (`v2/roots.rs`, libtest's harness in `v2/roots/harness.rs`),
> `HirFrontend::linked` and `link_inputs` with `LinkInputs { spans, debug_names }`
> (`v2/linked.rs`), `Built::reached` (`blaze-codegen/src/driver.rs`) and
> `NativeLib`/`NativeLibKind` (`blaze-codegen/src/lib.rs`). Nothing left without an owner.

> applied by codegen (lead, 2026-10-29): codegen owns them, and all are in: `v2::toolchain(target, rustflags, runs)` (`v2/toolchain.rs:126`), `RootKind`, `Roots` and `RootTest` (`v2/roots.rs`, re-exported in `v2.rs`), `HirFrontend::roots` (`v2/roots.rs:123`), `linked` and `link_inputs` (`v2/linked.rs:47`, `:52`). cargo calls each (`serve/unit/codegen.rs:295`, `:365`, `:504`). Its `toolchain` calls still pass two arguments: the `Runs` edit is asked in `v2/requests/codegen.md`, "From codegen-fc-host", and waits on codegen-rq-features.

## Done for the parent's codegen-changes.md (2026-10-29)

- Interface: `CompiledFunction::{unwind, lines}`, `Unwind`, `Static`/`Init`, `Target::Weak`
  (a static's `extern_weak` pointee) and the `DescribedStatic` fields were in already.
- 6, 7: `v2/convert.rs` maps Cranelift's TLS relocations (`ElfX86_64TlsGd`, `MachOX86_64Tlv`,
  `MachOAarch64TlsAdrPage21`/`PageOff12`, `Aarch64TlsDesc*`, `X86SecRel`) and ELF 19, 22, 23,
  541, 542, 549, 551 and the four TLSDESC types onto the new kinds, and ELF 277, 278, 284, 285, 286
  and 299 to `PageOffset12`. ELF 550 (`TLSLE_ADD_TPREL_LO12`, which checks for overflow) stays
  an error: `TpLo12` is the unchecked form.
- 8: the allocator shim is `Kind::Allocator(v2::allocator::Forward)`, one instance an entry
  (`__rust_alloc`, `__rust_dealloc`, `__rust_realloc`, `__rust_alloc_zeroed`,
  `__rust_alloc_error_handler`, `__rust_no_alloc_shim_is_unstable_v2`), its symbol in
  `Described::symbol`, calling `Target::Extern` of the `#[global_allocator]`'s export or of
  std's `__rdl_*`. `HirFrontend::allocator_shim(krate)` gives the entries and the
  allocator's exported functions as roots; whoever builds a program's roots (a `main` or a
  `cdylib`) adds them. Two `#[global_allocator]`s in one program are an error.
- 11: variable scopes are in the side table (version 2): `side::Scopes` is made at prepare
  time from the body (v1's `describe_scopes`), each scope's code from the line rows at emit,
  and the shape (version 4) holds them.
- 4: noted; `places.rs` still refuses writable data, and the programs it hits are reported
  when the tree first runs (nothing can run before then).

> applied: noted; a status report to codegen, nothing for the coordinator.

## To codegen-release: `side::encode` takes the scopes

`crates/hir-ty-codegen/v2/release.rs:263`:
```rust
let side = super::side::of(&function.variables, &function.scopes, code).map_err(|e| failed(unit, e))?;
```
(`side::of` gives the code's line rows and length to `side::encode`; the old two-argument
`encode` is gone.) Also, from the fast-code pass: `convert::function` takes the code by
value and moves its bytes into the `CompiledFunction` (no `bytes.clone()`). In `release.rs`,
`emitted` (`:255`) takes `code: crate::fragment::Code` and passes it to `convert::function`
after `side::of(.., &code)`; its callers at `:204` and `:245` hand over the code they own.
`places::resolve` gives a `&places::Placed` (a `convert::Resolver`, static dispatch), so the
closure there is unchanged.

> forwarded to codegen: between its children.

> applied by codegen-rq-features (2026-10-29), checked in `v2/release.rs`: `emitted` takes
> `code: crate::fragment::Code` by value. Inside `places::resolve_owning` (whose `FnOnce` moves
> it), it calls `side::of(variables, scopes, &code, resolve.frame())`, then
> `convert::function(.., code, ..)` with the side table's ID. Both callers (the assembly arm
> and the compiled arm of `emit`) hand over the code they own. No two-argument `encode` is
> left.

## Fast-code pass (2026-10-29): requests

1. **codegen (parent), `blaze-codegen`**: outside symbols as interned ids, not `String`s, in
   `FrontRef::Extern`/`Weak`, `Target::Extern`/`Weak` and `Described::symbol`. Today each
   describe allocates each symbol of its walk, and each relocation at one clones its `String`.
   `intern::Symbol` is a `Copy` pointer with an order key (`crates/intern/src/symbol.rs`), so
   compare, hash and sort stay what they are. And `Described::refs` borrowed from the front
   end's memo (a `Frontend<'f>` giving `&'f [FrontRef]`), so a describe copies no list.
2. **codegen (parent)**: the IR builder runs twice an instance, for the walk in `describe`
   and again in `prepare`. One build kept from `describe` to `prepare` (a `Described` that
   carries the prepared IR, or a `Frontend::describe` that is `prepare`'s first half) halves
   the front of the dev path.
3. **devspeed, `debuginfo.rs`**: `SlotPlaces::of` is a linear `find`, called once a local
   when the side table is written. Keep `slots` sorted by slot and `binary_search_by_key`,
   or index them by slot.
4. Done: `Homes::sees` as a salsa query (`home.rs` `sees`, see above).

Notes: `docs/v2/plan/lanes/codegen/adapter.md` names the benchmarks. The header of `release.rs` cites `docs/v2-codegen.md`, now
`docs/v2/plan/lanes/codegen.md`.

> forwarded to codegen: its lane decides which child owns these.

> answered by codegen-rq-lower (2026-10-29):
> - 1: not applied. `FrontRef`, `Target` and `Described` are the parent's (`blaze-codegen`),
>   and `Target` is in the store's key and bytes too. The parent decides. The borrowed
>   `Described::refs` half is met in the adapter: the walk is a memo kept for every revision
>   (`refs::code_walk`), and `prepare` borrows its list.
> - 2: not applicable. `describe` runs no IR builder. It reads the walk, a pass over the MIR
>   with `Build::WALK` and no Cranelift, memoized as above, so the IR is built once, in
>   `prepare`. Keeping a prepared IR in `Described` would make `describe` depend on the unit's
>   spec, which it must not.
> - 3: applied by codegen-rq-lower. `SlotPlaces` keeps `slots` sorted by slot, with first-told
>   order among equal slots (`SlotPlaces::new`, a stable sort that is skipped when they are
>   sorted, and the deserializer goes through it). `of` is a `partition_point`, so the first
>   one told for a slot is found, as the linear `find` found it. The test is
>   `a_slot_is_found_as_a_search_in_the_order_told_finds_it` (`debuginfo.rs`). The two other
>   builders, `llvm/emitted.rs` and `v2/backend.rs` `stencil_slots`, are asked to call `new`
>   (`v2/requests/codegen.md`, "From codegen-rq-lower").
> - 4: done.

## Round 2 (2026-10-29): devspeed items 1, 6 and 12, codegen-changes-2.md

Done in my paths: `Emitted::extra` and `summarize(.., &mut Prepared, ..)` (codegen-changes-2
sections 1 and 2) were in already. Concurrency finding 2 is gone: `Databases` has no `idle`, no
`Mutex` and no `Lent`; it is `&'db (dyn HirDatabase + Sync)`, shared, and each call makes its
own `Homes`. Devspeed item 1 holds: `backend::built` goes through `instance_code` and
`compile_as`, and `compile_as` calls `settle` before the Cranelift and the LLVM branch alike.

- `v2::build::Build` has `optimized: bool` (`Llvm { opt > 0 }`), `Build::WALK` and
  `Build::options(db, written, compiling) -> MirOptions`. The level is decided per body, not
  per unit: a library crate's body is at level 2 in any unit (as rustc ships its MIR), every
  other crate's at the unit's. UB checks are `sess.ub_checks()` of the crate whose MIR it is,
  which is the unit's own for the compiling crate. Lifetime markers are always on.
- The walk reads every body with `Build::WALK` (level 1, debug assertions on, unwinding). A body
  at level 1 asks for everything the same body at level 2 asks for, so one walk serves every
  unit and `describe` stays spec-free.
- Devspeed item 6: `refs::built_in(db, instance)` gives `Described::built_in`. Each built-in
  body's requests are listed among the caller's refs, through `declare_inlined` as the IR makes
  them. `Function::inlined` is the sorted code hashes of what is built in at any depth, and
  `emit_cranelift` puts it in `CompiledFunction::inlined`. Shapes carry it too (`shape`
  version 5): equal shapes are one template, and the template says what its code built in.

> applied: noted; a status report to codegen.

### To the owner of `crates/hir-ty-codegen/mod.rs` (speed-backend-lower / codegen-devspeed)

1. **`instance_body(db, instance, env, unit: Option<v2::build::Build>)`**, and the same `unit`
   for `compiled_mir_body`/`_for_closure`/the coroutine bodies. With `Some(unit)`, ask the
   optimized query with `unit.options(db, written, env.krate)`, where `written` is the crate
   whose MIR the body is. With `None`, keep v1's `mir_opt_level_of` rule. The IR builder passes
   `self.cg.unit`, and `v2/refs.rs` passes `Some(Build::WALK)`. Never read the thread's
   `building()` inside a query: a salsa memo can't see it.
2. **`FunctionCx::always_inlinable_callee(&self, def, args) -> Option<(Instance, &'db
   fragment::Inlinable)>`**: `always_inlined`'s conditions plus the `always_inlinable` query,
   with no signature check and no declaring. `always_inlined` becomes this, plus the signature
   check, plus `declare_inlined`. `v2/refs.rs` `Walk::instance_call` calls it, so the walk and
   the IR decide what is built in by one function.
3. **`always_inlinable(db, instance, env, unit: Option<Build>)`**: the built-in body depends on
   the unit's settings (debug assertions, panic strategy, MIR level), so they are part of the
   query's key. `built_in_body` sets `cg.unit = unit` on its `Recorder` in place of the
   thread's.
4. **`inlined_callee` for a v2 unit**: with `self.cg.unit.is_some()`, return
   `self.always_inlined(def, args, sig)`: the only callees a v2 unit builds in are the
   `#[inline(always)]` ones of crates no edit reaches, whose places the walk lists. `wasm` stays
   refused by `check_spec` (devspeed item 7).

> applied by codegen-rq-lower (2026-10-29), checked in `mod.rs`: all four are in.
> 1. `instance_body(db, instance, env, build: Option<v2::build::Build>)` (with the closure and
>    coroutine bodies) asks for `unit.options(db, written, ..)` when given `Some`, and v1's
>    rule when given `None`.
> 2. `always_inlinable_callee` is the one decision; `always_inlined` adds the signature check
>    and `declare_inlined`, and `Walk::instance_call` calls it.
> 3. `always_inlinable(db, instance, env, unit)` has the unit in the key, and `built_in_body`
>    sets `cg.unit`.
> 4. `inlined_callee` returns `always_inlined` for a v2 unit.
>
> No query reads `building()`.

### To codegen-release / speed-backend-llvm (`v2/release.rs`)

5. `emit` (around line 217) sets `emitted.function.inlined` from the imports alone, which drops
   what the function's own IR built in. Start from `function.inlined.clone()` (the
   `emitted(..)` result already carries it), push the imports' codes, then sort and dedup as
   now.

> applied by codegen-rq-features (2026-10-29), checked: `emit` collects the imports' code
> hashes, then `inlined.extend_from_slice(&function.inlined)`, `sort_unstable` and `dedup`, and
> sets `emitted.function.inlined` to that. It reuses the vector, with no clone of
> `function.inlined`.

### To codegen-mir (`hir-ty-mir/src/mir/optimize.rs`)

6. Keep, and say in `optimize`'s doc, that a pass run only at `MirOptLevel::Release` never adds a
   call, constant, static or vtable the body did not have at `Debug`. The v2 refs walk reads
   `Debug` bodies for every unit and relies on this (`v2::build::Build::WALK`). A differential
   test that the referenced items at `Release` are a subset of those at `Debug`, over the
   `hir-ide` MIR fixtures, would hold the property.

> applied by codegen-mir (2026-10-29): said in `optimize`'s doc; `hir-ty-mir/tests/mir/references.rs`
> checks it (constants, statics, drops, coercions, closures) with `inline` off. Once the inliner
> is registered, an inlined body's references are its callees', so the walk keeps the property
> only transitively: it must visit each `inlined.callees` body as it visits a call.

### To codegen (parent)

7. `Build` carries `optimized`, not one level, because rustc's level is the body's crate's (a
   std generic in a user crate is at std's level 2). `Build::options` is the one place the
   rule lives.

## Round 3 (2026-10-29): speed-backend-debug, speed-backend-lower, codegen-hashes

- speed-backend-debug 1 (Context reuse) and speed-backend-lower 2: done. `compiled` runs in
  `crate::with_context`.
- speed-backend-lower 1: done. `ty_layout` lends `&'cx Layout`, and nothing imports `triomphe`.
- speed-backend-debug 2 (CIE once): `v2/unwind.rs` makes and reads back the CIE once per process
  (`Common`, a `OnceLock`). It is keyed by the process's ISA, the only one a v2 unit compiles
  with, and another ISA is an error. Per function only the FDE is read back. Still open, to
  speed-backend-debug: `FunctionUnwind::v2_fde(&self, code_align, data_align, out)`, which
  writes the FDE's call frame instructions with no gimli `FrameTable` and no CIE clone per
  function. Say in your window when it lands; `Common::fde` becomes one call of it.
- To codegen (parent): `Unwind::Dwarf::cie` is a `Vec<u8>` per function, the same bytes for
  every function of an ISA. A `ContentId` of the CIE, or the CIE carried once per build
  (`Spec` or the linker's input), would remove one allocation and copy per function.
- codegen-hashes (`ItemHashes::ty`/`konst` refuse aliases, placeholders, inference variables,
  unevaluated and expression constants): checked every caller in `v2/`.
  - Instance args (`frontend.rs` `args_of`): every `Kind::Code` instance is made by
    `places::instance_ref` or `HirFrontend::root`, which has no args. `instance_ref` passes its
    args through `places::evaluated`, so an instance is also one key however its args were
    spelled. `evaluated` is the next solver's `deeply_normalize` in `TypingMode::PostAnalysis`
    with an empty param env (rustc's `fully_monomorphized`), which evaluates unevaluated
    constants. Nothing is made when nothing has aliases.
  - `TypeId`s (`places.rs` `fill_type_ids`): through `evaluated` in the crate whose code holds
    the constant. `place_requests` and `fill_type_ids` take `krate: Option<Crate>`, which is
    `backend::compiled_in` for a function, the static's crate for a static, and `None` for
    assembly.
  - Named locals (`backend.rs` `named`): through `evaluated` in the environment the IR
    builder recorded with them.
  - `refs.rs` calls no hash itself: its instances come from `instance_ref`.
  - Monomorphization (`hir-ty-mir` `Filler`) normalizes aliases at any depth and refuses what
    it can't normalize, but only substitutes constants. That is why `evaluated` is needed. A
    `ConstKind::Expr` left by `generic_const_exprs` after substitution is not an alias, so it
    still reaches `ItemHashes` and fails loudly. To codegen-mir: evaluate `Expr` constants in
    `Filler::try_fold_const` once their operands are values, as rustc's
    `normalize_erasing_regions` does.

> applied: noted; a status report to codegen.

> applied by codegen-rq-lower (2026-10-29): `v2_fde`, which was still open, is in as
> `unwind::Fde::write` (see "From speed-backend-debug" 2). `Common::fde` becoming one call of
> it is asked of codegen-rq-columns. Not applied: the CIE once per build. `Unwind::Dwarf::cie`
> is the parent's type (`blaze-codegen`), and it is in the stored function's bytes.
> Carrying a `ContentId` or the CIE in `Spec` is the parent's to decide. Today it costs one
> clone of about 20 bytes per function (`common.entry.cie.clone()`). The codegen-hashes
> finding about `Expr` constants is codegen-mir's.

## Round 4 (2026-10-29): speed-backend-sanitize Z3 (`v2/requests/codegen.md`)

Done, in `v2/backend.rs`, `v2.rs`, `v2/shape.rs`, `v2/convert.rs`, `v2/frontend.rs` and tests:

1. `check_spec` refuses `Backend::Cranelift` with a non-empty `spec.sanitize`, with the message
   asked for.
2. `Function::unsanitized` is set by `backend::unsanitized`. With an empty set it is `NONE`,
   so a plain build's templates are unchanged. Otherwise it is
   `hir_def::attrs::disabled_sanitizers` of the function rustc takes the instance's attributes
   from (`codegen_instance_attrs` reads a shim's `def_id`), which is `frontend::item_of`:
   - a function or vtable shim: the function;
   - a closure or coroutine: its owner;
   - drop glue: `drop_in_place`;
   - a vtable `call_once` and a fn-pointer shim: `FnOnce::call_once`;
   - a reify shim: the function it reifies.

   This differs from the request, which said shims get `NONE`. A vtable or reify shim of a
   function has that function's set in rustc. The C `main`, the allocator shim and `asm!` are
   no item and get `NONE`. The set is masked to the unit's sanitizers, so a bit the unit
   doesn't use never splits templates. `Sanitizers::COVERAGE` is added when coverage is on and
   `symbol_of` starts with `__sanitizer_`.
3. `shape.rs`: `part::UNSANITIZED = 12` holds the bits after `HINT`, and `VERSION` is 6.
4. `convert.rs`: `RelocName::User { namespace: llvm::sanitize::COVERAGE, index }` maps to
   `Target::Coverage(index)` for `0..3` and is refused past that (behind `feature = "llvm"`,
   since only LLVM emits it). The function it builds has `coverage: None`.
5. Every `Spec` literal in my tests has `sanitize: Sanitize::default()`.
6. Tests:
   - `a_functions_sanitizers_off_are_its_nearest_attributes` covers own, inherited from a
     module, nearest `= "on"`, a closure, masking, the `__sanitizer_*` callback and no set.
   - `what_is_unsanitized_is_in_the_shape`.
   - `cranelift_refuses_a_sanitized_unit`.
   - `convert::tests::a_coverage_relocation_is_at_the_functions_own_array`.

These build once Z1, Z2 and db's `disabled_sanitizers` land.

> applied: by codegen-adapter, as it reports; nothing for `requests`.

### To db: `disabled_sanitizers` of any item, not only a function

`v2/requests/db.md` "From speed-backend-sanitize" takes a `FunctionId`, but rustc's
`sanitizer_settings_for` takes any `LocalDefId`. Under a sanitizer, the adapter refuses two
kinds of instance whose item isn't a function, naming the item:

- a reify shim of a tuple struct or enum variant constructor, whose set comes from the
  struct, its module and the crate;
- a closure in a `const`, `static` or enum discriminant, whose set comes from that item.

Asked: `disabled_sanitizers(db, def: AttrDefId) -> u8` (or a `GenericDefId`/`DefWithBodyId`
over the same parent walk), so `backend::unsanitized` can read the same set for every
`ItemId` `item_of` gives. Until then those instances are refused, never given a guessed set.

> written by codegen-adapter (2026-10-29).
