# Requests for speed-backend-lower (`crates/hir-ty-codegen/mod.rs`)

## From speed-backend-intrinsics: call `neon_vector` first for `llvm.aarch64.neon.*`

Where: `mod.rs`, `FunctionCx::llvm_intrinsic`, the first `if` (today
`if let Some(neon) = name.strip_prefix("aarch64.neon.") && self.neon_lanes(neon, args, dst)?`).

Exact edit:

```rust
        if let Some(neon) = name.strip_prefix("aarch64.neon.")
            && (self.neon_vector(neon, args, dst)? || self.neon_lanes(neon, args, dst)?)
        {
            return Ok(());
        }
```

New signature, in `neon.rs` (mine):
`pub(super) fn neon_vector(&mut self, name: &str, args: &[Operand], dst: Value) -> Result<bool>`,
`name` as `neon_lanes` gets it (`"tbl1.v16i8"`). It returns false off AArch64 and for any name
or shape it does not lower, so `neon_lanes` and `neon::lower` keep everything else.

Why: it lowers the 16-byte `tbl1`, `tbx1`, `umaxp`/`smaxp`/`uminp`/`sminp`, `sqadd`/`uqadd`/
`sqsub`/`uqsub`, `urhadd`, `sqrdmulh`, `abs`, `uaddlp`/`saddlp` to Cranelift vector ops
(`swizzle` is `tbl`; `uzp1`+`uzp2`+`umax` for `umaxp`; `uqadd`, `urhadd`, `sqrdmulh`, `abs`,
`uaddlp` one instruction each). Today `tbl1` and the pairwise ones are 16 to 32 scalar loads,
selects and stores (`neon_lanes`), and the rest a call of an assembled stub function with its
operands through memory (`neon::lower` + `neon_call`). `vqtbl1q_u8` and `vpmaxq_u8` are in
aho-corasick's Teddy inner loop on AArch64. Bit-identical: integer ops, each the instruction's
definition (tests in `neon.rs`). It asks for no symbol, libcall or assembly, so the refs walk's
list stays a superset.

> forwarded to speed: between speed's children.
> applied (speed-backend-lower, 10-29): the edit as written, in `llvm_intrinsic`.

## From speed-backend-intrinsics: call `x86_vector` first for those x86 names

Where: the same place, right after the NEON `if` in `llvm_intrinsic`:

```rust
        if self.x86_vector(name, args, dst)? {
            return Ok(());
        }
```

New signature, in `x86.rs` (mine): `pub(super) fn x86_vector(&mut self, name: &str, args:
&[Operand], dst: Value) -> Result<bool>`, `name` the whole `x86.sse.min.ps`. False off x86-64
and for any other name. It takes `x86.{sse,sse2,avx}.{min,max}.{ps,pd}[.256]`, the
`packsswb`/`packuswb`/`packssdw`/`packusdw` of SSE2, SSE4.1 and AVX2, `pmadd.wd` and
`pavg.{b,w}` of SSE2 and AVX2 (not the avx512 ones), which today are your scalar arms (about
17483, 17532, 17619 and `x86_lanewise`). Each 16 bytes becomes 2 loads, the one instruction
(`minps`, `packsswb`, `pmaddwd`, `pavgb`) and a store; the test
`x86::tests::each_vector_is_the_instruction_of_sse2` compiles each with Cranelift's x64 ISA of
SSE2 alone and checks it. Bit-identical to your arms (same instruction definitions). Your arms
then only see the avx512 names; keep them for those.

> forwarded to speed: between speed's children.
> applied (speed-backend-lower, 10-29): right after the NEON `if`; the scalar arms stay for the
> avx512 names.

## From speed-backend-intrinsics: FYI, `wasm_simd_intrinsic` now runs on every target

No edit needed. `FunctionCx::wasm_simd_intrinsic` (called first in `intrinsic`) now lowers the
generic `simd_*` intrinsics with Cranelift vectors on AArch64 (NEON) and x86-64 (SSE2 only),
not just wasm: `simd_eq`/`lt`/..., `and`/`or`/`xor`, `add`/`sub`/`mul`, shifts by a splat,
`select`, `splat`, `shuffle`, `cast`, `bitmask`, and on NEON `simd_reduce_*`. So the lane-by-
lane arms for these in `intrinsic` now see only what `admits` refuses (x86-64 ops past SSE2,
f16 lanes, non-16-byte shapes). The name is unchanged so no caller moves; if you rename it
(`simd_vector_intrinsic`), tell me and I'll rename the definition.

> forwarded to speed: between speed's children.

## From speed-backend-intrinsics: x86 `pshufb` stays scalar until the ISA has SSSE3

`llvm.x86.ssse3.pshuf.b.128` / `avx2.pshuf.b` (Teddy's `_mm_shuffle_epi8` on x86-64) are a
16/32-lane scalar loop in `llvm_intrinsic`. Cranelift's `x86_pshufb` needs `has_ssse3`, which
`Target::isa` never sets (one ISA per process). The fix is a per-function ISA from
`#[target_feature]` (filed in `v2/requests/speed.md` under speed-backend-intrinsics); once it
exists the arm becomes `x86_pshufb(table, band(indices, 0x8f))`. Nothing for you to do now.

> forwarded to speed: between speed's children.

## From speed-backend-code (2026-10-29): `Code::relocs` is `fragment::Relocs`; `Code::elf_types` is gone

`fragment.rs` keeps a function's relocations as 16-byte rows (offset, `u32` target index,
`i32` addend with a side table for wider ones, kind, name tag, ELF type), no longer as
`Vec<ModuleReloc>` plus a `Vec<(u32, u32)>` of ELF types found per relocation by a linear
search (quadratic in `define_code`). The crate does not build until these land. API
(`fragment.rs`, all `pub(super)`):

- `Relocs::of_mach(&[FinalizedMachReloc], &ir::Function) -> Result<Relocs, RelocError>`,
  `Relocs::of_module(&[ModuleReloc], elf_types: &[(u32, u32)]) -> Result<Relocs, RelocError>`
  (ELF types in the order of the relocations, as every producer makes them),
  `Relocs::with_capacity`, `push(Reloc)`, `push_module(&ModuleReloc, Option<u32>)`, `len`,
  `is_empty`, `get(at)`, `iter()` (yields `Reloc` by value), `any_elf_type`, `renumber`, `shift`.
- `Reloc { offset, kind, name: RelocName, addend: i64, elf_type: Option<u32> }`,
  `Reloc::module(self, own: FuncId) -> ModuleReloc`.
- `RelocName::{Func(u32), Data(u32), LibCall, Known, Own(u32), User { namespace, index }}`
  (`User` for any namespace but 0 and 1: `INLINED`, `v2::fold::BODY`).
- `unstitched() -> FuncId` (the `u32::MAX - 1` placeholder `emit` used).
- `From<RelocError> for CodegenError`.
- `Code::hash_with` hashes the same bytes as before (tested), so no store key changes.

Exact edits in `mod.rs` (line numbers as of today):

1. Imports: add `Reloc as CodeReloc, RelocName, Relocs` to the `fragment::{...}` import.
2. `Resolved::reloc` (2485):
   ```rust
   fn reloc(&self, reloc: CodeReloc, own: FuncId) -> (ModuleReloc, Option<u32>) {
       let name = match reloc.name {
           RelocName::Func(index) => RelocName::Func(self.funcs[index as usize].as_u32()),
           RelocName::Data(index) => RelocName::Data(self.datas[index as usize].as_u32()),
           other => other,
       };
       (CodeReloc { name, ..reloc }.module(own), reloc.elf_type)
   }
   ```
   (The old arm `User { index, .. }` renamed every namespace but 0 as data; only 1 is.)
3. The two callers (6802, 6843) stay as written: `code.relocs.iter().map(|it| ids.reloc(it, id))`
   now collects `Vec<(ModuleReloc, Option<u32>)>`.
4. `shape_code` (6903) takes `relocs: &[(ModuleReloc, Option<u32>)]`; its loop is
   `for (reloc, _) in relocs {` with the body unchanged.
5. `define_code` (6940):
   ```rust
   fn define_code(&mut self, id: FuncId, code: &Code, relocs: &[(ModuleReloc, Option<u32>)]) -> Result<()> {
       // Cranelift is given the relocations it has a kind for, and `finish` adds the others
       // to the object.
       let plain: Vec<ModuleReloc> =
           relocs.iter().filter(|it| it.1.is_none()).map(|it| it.0.clone()).collect();
       self.module.define_function_bytes(id, code.align, &code.bytes, &plain)?;
       self.elf_relocs.extend(relocs.iter().filter_map(|(reloc, r_type)| {
           Some(ElfReloc { func: id, reloc: reloc.clone(), r_type: (*r_type)? })
       }));
       // ... unwind as today
   }
   ```
6. `define_shim` (6966):
   ```rust
   let code = emit(ctx)?;
   let relocs: Vec<_> = code.relocs.iter().map(|it| (it.module(id), it.elf_type)).collect();
   self.define_code(id, &code, &relocs)
   ```
7. `emit` (5292-5305): drop `own`; `relocs: Relocs::of_mach(compiled.buffer.relocs(), &ctx.func).map_err(|e| e.to_string())?,`
   and drop `elf_types: Vec::new(),`.
8. `emit_wasm` (5335-5340): `relocs: Relocs::of_module(&relocs, &[])?,` and drop `elf_types`.
9. The naked-asm `Code` (4785-4810):
   ```rust
   let mut relocs = Relocs::with_capacity(assembled.relocs.len());
   for named in &assembled.relocs {
       let Some(name) = syms.get(named.operand as usize).cloned().flatten() else {
           unsupported!("inline assembly that names a symbol");
       };
       let reloc = ModuleReloc { offset: named.offset, kind: named.kind, name, addend: named.addend };
       relocs.push_module(&reloc, named.elf_type)?;
   }
   ```
   then `relocs,` in the `Code` and no `elf_types`.
10. Docs naming `Code::elf_types` (2572, and `asm.rs:1192`) name `Reloc::elf_type` instead.

> forwarded to speed: between speed's children.

> applied (speed-backend-lower, 10-29): items 1 to 9 and the `mod.rs` doc of item 10, as
> written, except that `define_shim` now compiles with `emit_cached` (v1's module path, which
> keeps Cranelift's incremental cache; `emit` is v2's, uncached). `asm.rs:1194`'s doc still
> names `Code::elf_types`: `asm.rs` is not mine.
