# rsasm

[![CI](https://github.com/KarpelesLab/rsasm/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/rsasm/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rsasm.svg)](https://crates.io/crates/rsasm)
[![docs.rs](https://img.shields.io/docsrs/rsasm)](https://docs.rs/rsasm)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An assembler written in Rust, aiming at three things at once: accept the asm
text people actually have, target many CPUs, and let one source file emit code
for more than one of them.

```console
$ cat hello.s
        .section .rodata
msg:    .ascii  "Hello from rsasm!\n"
msglen = . - msg

        .text
        .globl  _start
_start:
        movq    $1, %rax                # write
        movq    $1, %rdi                # stdout
        leaq    msg(%rip), %rsi
        movq    $msglen, %rdx
        syscall
        movq    $60, %rax               # exit
        xorq    %rdi, %rdi
        syscall

$ rsasm -o hello.o hello.s && ld -o hello hello.o && ./hello
Hello from rsasm!

$ rsasm --link -o hello hello.s && ./hello   # or link it without leaving rsasm
Hello from rsasm!
```

## Install

```console
$ cargo install rsasm
```

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/KarpelesLab/rsasm/releases).

## Status

Early, but broad. The pipeline is complete end to end — lex, parse, encode,
lay out, relax, relocate, write — with sixteen backends behind it.

### Architectures

Every encoding claimed below is checked byte for byte against an independent
assembler, not against rsasm's own idea of the manual. The count is the
target's cases in the three instruction-level harnesses — `tools/gas-diff`,
`tools/mc-diff` and `tools/xas-diff` — which is how a row can be recomputed
after the corpora grow; the whole-object, flat, link and fuzzing harnesses in
[Verification](#verification) add far more.

| Target | Names | Checked against | Cases |
|---|---|---|---|
| x86-64, i386, i8086, with x87, MMX, 3DNow!, SSE–SSE4.2, AVX, AVX2, AVX-512 with every subset and FP16, AVX10.2, FMA4, XOP, BMI, AMX, CET, Key Locker, APX | `x86-64` `i386` `i8086` | GNU as, llvm-mc | 24860 |
| AArch64, with AdvSIMD (NEON), the cryptographic extensions, SVE and SVE2 up to SVE2.3, the exclusives and the LSE atomics, the floating-point atomics, pointer authentication, memory tagging, the CRC32 checksums, the unprivileged and unscaled accesses, the acquire-release pair, the condition-flag instructions, the memory copies and sets, compare-and-branch, the 64-byte accesses, the guarded call stack, transactional memory, the system instructions and literal pools | `aarch64` | llvm-mc, GNU as | 24822 |
| ARM A32 / Thumb, with the floating-point unit (VFPv4) and NEON | `arm` `thumb` | llvm-mc, GNU as | 3252 |
| RISC-V RV32/RV64 IMAFDC | `riscv32` `riscv64` | llvm-mc | 582 |
| PowerPC 32/64, both endians, with AltiVec, VSX and POWER8–10 | `powerpc` `powerpc64` `powerpc64le` | llvm-mc, GNU as | 9545 |
| MIPS 32/64, both endians | `mips` `mipsel` `mips64` `mips64el` | llvm-mc | 880 |
| SPARC V8 / V9 | `sparc` `sparcv9` | llvm-mc | 344 |
| m68k: 68000–68060, CPU32, 68881/68882, 68851, ColdFire, GNU and Motorola syntax | `m68k` `68000` … `68060` `cpu32` `5475` … | GNU as, vasm | 3791 |
| SuperH SH-1 to SH-4A, both endians | `sh` `shl` | GNU as | 1285 |
| Renesas RX (RXv1), GNU and CC-RX syntax | `rx` | GNU as | 647 |
| Renesas RL78, GNU and CC-RL syntax | `rl78` | GNU as | 531 |
| TI MSP430 and MSP430X | `msp430` `msp430x` `msp430xv2` | GNU as | 5560 |
| NEC/Renesas V850 and RH850, GNU and CC-RH syntax | `v850` `rh850` | GNU as | 565 |
| NEC 78K0, in CA78K0 syntax | `78k0` | AS | 1276 |
| Microchip AVR, every core GNU as knows | `avr` `avr1`–`avr6` `avrxmega2`–`avrxmega7` `avrtiny` | GNU as | 1940 |
| Zilog Z80, with the undocumented `IXH`/`IXL` forms, Zilog and GNU syntax | `z80` | GNU as, vasm | 2572 |
| MOS 6502, in ca65 syntax | `6502` | ca65, vasm | 551 |
| Intel 8080, in Intel mnemonics | `i8080` | AS | 278 |
| Intel 8051 (MCS-51), in Intel mnemonics | `8051` | AS, sdas8051 | 960 |

The 8-bit targets are checked against the assemblers their source is written
for: cc65's ca65 for the 6502, GNU as and vasm for the Z80, and the Macro
Assembler AS for the 8080 — GNU as has no Intel mnemonics, and vasm's `RST`
takes a Zilog address — and for the 8051, AS and SDCC's sdas8051. Tests also
walk each complete opcode space and assert that exactly the documented
encodings exist. They are for flat binaries, or Intel HEX; ELF has no class
for a 16-bit target. See [the 8-bit dialect](#the-8-bit-dialect).
The 78K0's table was extracted from NEC's instruction manual, checked against
the byte counts in a second NEC manual, and cross-checked against MAME's
disassembler, which agrees on all but 18 forms where both manuals show MAME to
be wrong. CA78K0 itself is a proprietary Windows tool, but AS assembles the
family too, under the CPU name `78070`, so the table is checked against it
form by form and in random whole programs as well.

### Everything else

**Working**

- AT&T and Intel syntax on x86, switchable mid-file; `.code16`/`.code32`/`.code64`
  and `.code16gcc`
- NASM source (`-d nasm`): its preprocessor (`%macro`, `%rep`, `%if`, `%define`,
  `%assign`, contexts, `%include`), `db`/`resb`/`times`/`equ`/`struc`, sections
  with attributes, `default rel`, and NASM's operand syntax and `wrt`
  relocations; see [Dialects](#dialects)
- several targets in one file, switched with `.arch`; see
  [Multi-architecture files](#multi-architecture-files)
- ELF relocatable objects, 32- and 64-bit, REL or RELA as each psABI requires,
  flat binaries, and flat images as Intel HEX (`-f ihex`); a GOT load on one
  of the instruction forms a linker may rewrite into a direct reference takes
  the relaxable relocation GNU as gives it — `R_386_GOT32X`, or
  `R_X86_64_GOTPCRELX` and its REX form
- MIPS position-independent code, which reaches everything outside the object
  through a global offset table `$gp` points into: the operators `%got`,
  `%call16`, `%got_disp`, `%got_page`, `%got_ofst`, `%got_hi`, `%got_lo`,
  `%call_hi`, `%call_lo`, `%gp_rel` (also spelled `%gprel`) and `%neg`, and
  with them the rest of what GNU as's operand parser reads — `%half`, and
  `%higher` and `%highest` for the two fields above `%hi` that a 64-bit
  address needs — each in any 16-bit field, an instruction's own immediate or
  a memory operand's displacement; the nesting that composes them, which is
  grammar rather than a special case, since an n64 `r_info` holds three
  relocation types and `%hi(%neg(%gp_rel(f)))` fills all three; the `$gp`
  prologue directives, each the instruction sequence GNU as expands it to —
  `.cpload` and `.cprestore` in o32, `.cpsetup`, `.cpreturn` and `.cplocal`
  in n32 and n64, with the ones belonging to the other ABI read and ignored
  as both references read them; `.abicalls` and `.option pic2`, which turn
  all of that on and set `EF_MIPS_PIC`, and `.option pic0`, which takes it
  back; and `.gpword` and `.gpdword`, a symbol's distance from `_gp` in four
  bytes or eight, which outside position-independent code are the plain
  absolute words GNU as makes them
- `x@SIZE` on x86, the size of a symbol rather than its address:
  `R_386_SIZE32`, `R_X86_64_SIZE32` and, in an eight-byte field,
  `R_X86_64_SIZE64`. A symbol the file defines and does not export has a size
  nothing can change afterwards, so GNU as works it out and writes the number
  with no relocation at all; that is what rsasm does, down to the `.size`
  given after the reference and the one a `.set` alias carries from the
  symbol it names
- thread-local storage: a symbol defined in a section carrying `SHF_TLS` is
  `STT_TLS` whatever a `.type` said, on every target; `.tdata` and `.tbss`
  are such sections whether the flag string or the built-in name says so, and
  `.tls_common` declares a thread-local common block. x86-64 and i386
  assemble the access models GNU as accepts for each — `@TLSGD`, `@TLSLD`,
  `@TLSLDM`, `@DTPOFF`, `@TPOFF`, `@NTPOFF`, `@GOTTPOFF`, `@GOTNTPOFF`,
  `@INDNTPOFF`, `@TLSDESC` and `@TLSCALL` — in code and in data, each only in
  the instruction forms a linker knows how to rewrite. AArch64 does the same
  with its operators: the local-exec and local-dynamic offsets (`:tprel_g2:`
  to `:tprel_g0_nc:`, `:tprel_hi12:`, `:tprel_lo12:`, `:tprel_lo12_nc:` and
  the `:dtprel_*:` twins), initial exec (`:gottprel:`, `:gottprel_lo12:`,
  `:gottprel_g1:`, `:gottprel_g0_nc:`), general and local dynamic
  (`:tlsgd:`, `:tlsgd_lo12:`, `:tlsldm:`, `:tlsldm_lo12_nc:` and their
  move-wide forms), TLS descriptors (`:tlsdesc:`, `:tlsdesc_lo12:`,
  `:tlsdesc_off_g1:`), the `.tlsdesccall`, `.tlsdescadd` and `.tlsdescldr`
  marks, and `.xword %dtprel(sym)`, each on the instructions GNU as takes it
  on; none has an ILP32 form here, since the backend writes LP64 objects
  only. ARM and Thumb assemble GNU as's — `(TLSGD)`, `(TLSLDM)`, `(TLSLDO)`,
  `(GOTTPOFF)`, `(TPOFF)` and `(TLSDESC)` in data, with the distance from the
  `add` a compiler writes after them, `bl sym(tlscall)`, and the
  `.tlsdescseq` mark on the instructions of a descriptor sequence. PowerPC
  assembles them for both word sizes: `@tprel` and `@dtprel`
  with every half the object has (`@l`, `@ha`, `@higher`…), their DS forms
  and, on POWER10, the 34-bit field; the GOT entries `@got@tprel`,
  `@got@dtprel`, `@got@tlsgd` and `@got@tlsld` and their halves, with the
  prefixed `@got@…@pcrel` forms; `@dtpmod`, `@tprel` and `@dtprel` in a
  pointer-sized data word; and the markers that cover no field, `add rD, rA,
  sym@tls` (and `@tls@pcrel`) on the instructions that take it and the
  `(sym@tlsgd)` or `(sym@tlsld)` argument of `bl __tls_get_addr`, which comes
  out as a relocation ahead of the call's own. RISC-V has the four models the
  psABI names, in RV32 and RV64: local exec (`%tprel_hi`, `%tprel_lo` and the
  `%tprel_add` that marks the `add` of the thread pointer), initial exec
  (`%tls_ie_pcrel_hi`), general dynamic (`%tls_gd_pcrel_hi`) and descriptors
  (`%tlsdesc_hi`, `%tlsdesc_load_lo`, `%tlsdesc_add_lo` and the
  `%tlsdesc_call` mark on the `jalr`), each `auipc` form completed by the
  same `%pcrel_lo(label)` an ordinary PC-relative pair takes, with
  `la.tls.ie` and `la.tls.gd` expanding to such a pair; there is no
  thread-local modifier in a RISC-V data word, since GNU as has none. MIPS
  reads the seven operators
  GNU as has, in o32 and n64 alike and in any 16-bit field: `%tlsgd` and
  `%tlsldm` for the GOT entries `__tls_get_addr` is given, `%gottprel` for
  the one initial exec reads, `%dtprel_hi`/`%dtprel_lo` for an offset within
  a module's block and `%tprel_hi`/`%tprel_lo` for one from the thread
  pointer; an n64 `r_info` holds one of them and leaves its other two types
  `R_MIPS_NONE`, which is what both references compose. Its three data
  directives are there too — `.dtprelword` and `.tprelword` for a four-byte
  offset and `.dtpreldword` for an eight-byte one, which a `.word` cannot
  spell since both references refuse an access-model operator in one.
  SPARC reads the eighteen operators its GNU as reads, the same in V8 and
  V9: the local-exec halves `%tle_hix22()` and `%tle_lox10()`, initial
  exec's `%tie_hi22()` and `%tie_lo10()`, general dynamic's `%tgd_hi22()`
  and `%tgd_lo10()`, and local dynamic's `%tldm_hi22()`, `%tldm_lo10()`,
  `%tldo_hix22()` and `%tldo_lox10()`, each of which names a step of a model
  rather than a part of a value and so goes in whichever field the
  instruction has; and the eight
  that fill no field at all and are written after the last operand —
  `%tie_ld()`, `%tie_ldx()`, `%tie_add()`, `%tgd_add()`, `%tldm_add()`,
  `%tldo_add()`, and `%tgd_call()` and `%tldm_call()`, which take the place of
  the displacement of the `call __tls_get_addr` they mark instead of sitting
  beside it, since the linker finds the function by name. On every target a
  thread-local model on a symbol defined outside a thread-local section is
  refused, as GNU as refuses it
- a relocation modifier the target has no relocation for is refused, in an
  instruction operand as well as in a data directive, rather than relocated
  as if it were not written: `movl x@FOOBAR(%rip), %eax` is an error, where
  writing the plain `R_X86_64_PC32` would have been a different program
- PE/COFF relocatable objects for x86-64, i386 and ARM64 (`-f coff`, or NASM's
  `-f win64` and `-f win32`): COMDAT sections, weak externals, `.def`, `.rva`,
  `.secrel32` and `@IMGREL`, x86-64 unwind data from `.seh_*`, DWARF, and
  CodeView line and file information from `.cv_*`; see [PE/COFF](#pecoff)
- Mach-O relocatable objects for x86-64, i386 and arm64 (`-f macho`, or a
  Darwin triple such as `-a arm64-apple-macos`), with Darwin's section, symbol
  and data-in-code directives and its `__DWARF` segment, byte for byte as
  llvm-mc writes them; see [Mach-O objects](#mach-o-objects)
- branch relaxation, alignment, `.org`, symbol arithmetic, conditionals
- macros: `.macro` with defaults, `:req` and `:vararg`, plus `.rept`, `.irp`,
  `.irpc`, `.exitm` and `.purgem`
- DWARF: line tables from `.file` and `.loc`, versions 2 to 5, call frame
  information from `.cfi_*` in `.eh_frame` or `.debug_frame`, and `-g` to
  describe the assembly source itself, in ELF, PE/COFF and Mach-O objects
  alike; see [Debug information](#debug-information)
- each target's own comment syntax, so ARM's `@`, AArch64's `//` and SPARC's
  `!` work, and `#` stays an immediate prefix where it is one
- the x86 instruction-set extensions both GNU as and llvm-mc assemble: AVX-512
  and all its subsets (BW, DQ, CD, IFMA, VBMI, VBMI2, VNNI, BITALG, VPOPCNTDQ,
  VP2INTERSECT, BF16, FP16, ER, PF) with writemasks, `{z}`, `{1toN}`, rounding
  and disp8\*N at every tuple type, AVX10.2, the VEX additions (F16C, FMA,
  GFNI, VAES, VPCLMULQDQ, SHA, SHA512, SM3, SM4, AVX-VNNI, AVX-IFMA,
  AVX-NE-CONVERT, AVX-VNNI-INT8/16), FMA4 and XOP, BMI1/2, TBM, LWP, AMX, CET,
  Key Locker, and the newer system instructions; APX -- `r16`-`r31` under the
  REX2 prefix, the new destination register, `{nf}`, `ccmp`/`ctest` with their
  `{dfv=...}` mask, `cfcmov`, `setzu`/`imulzu`, the paired and hinted pushes
  and `jmpabs`; the named compare predicates (`vcmpneq_oqps`, `vpcmpnltuq`),
  AT&T length spellings (`vcvtpd2psx`) and the `{vex}`, `{vex3}`, `{evex}`,
  `{rex}`, `{rex2}` and `{nf}` pseudo-prefixes
- ARM and Thumb as GNU as assembles them: literal pools (`ldr r0, =x`,
  `.ltorg`) down to the four-byte slots GNU as keeps them in -- the byte and
  halfword loads take an entry as well, `vldr d0, =x` takes two slots and
  aligns the pool to eight, that eight-byte entry taking a difference of
  labels GNU as's parser folds to a number as readily as a written one, and a
  number a `mov`, `mvn`, `movw`, `vmov.i64`, `vmov.f32` or `vmov.f64` can
  hold is moved instead of loaded --
  the PC-relative loads that name a label rather than a pool entry
  (`ldr r0, label`, the byte, halfword, doubleword and preload forms, and in
  ARM state the stores as well), which in Thumb pick between a 16-bit form
  reaching a word-aligned label 1020 bytes ahead and a 32-bit one reaching
  4095 bytes either way,
  `adr` and `adrl`, `it` blocks -- written, or made up for a conditional
  instruction that has none of its own as `-mimplicit-it` asks, each
  instruction after the first widening the `it` it joins --
  `.thumb_func`, `.thumb_set` and calls between
  the two instruction sets, the position-independent operands
  (`.word sym(GOT)`, `(GOTOFF)`, `(GOT_PREL)`, `(PLT)`, `(SBREL)`,
  `(TARGET1)`, `(TARGET2)`,
  `.word _GLOBAL_OFFSET_TABLE_`, `bl sym(PLT)`, and
  `movw`/`movt` with `:lower16:` and `:upper16:`, absolute or measured
  against a label), the thread-local ones (`.word sym(TLSGD)` and the other
  access models, `bl sym(tlscall)` and `.tlsdescseq`), `$a`/`$t`/`$d`
  mapping symbols, and the
  `.ARM.attributes` section the linker reads to decide what the program may
  contain — without it GNU ld assumes the oldest architecture and routes
  every interworking call through a veneer; the whole
  ARMv7-A/R/M instruction set with the security, virtualization and divide
  extensions, and with it the floating-point unit up to VFPv4 and NEON --
  the vector arithmetic over `d` and `q` registers, the shifts, the widening
  and narrowing forms, scalars and lanes, `vmov`'s modified immediate with
  the `cmode` GNU as picks for it, `vldm`/`vstm`/`vpush`/`vpop`, and the
  `vld1`-`vld4` and `vst1`-`vst4` structure transfers with their lists,
  alignments and lane indices; and the `.ARM.attributes` GNU as writes into
  every ARM object, without which `objdump` disassembles it against a default
  CPU -- with `.arch`, `.cpu`, `.fpu`, `.arch_extension`, `.object_arch` and
  `.eabi_attribute` to change it
- AArch64 the same way: literal pools (`ldr x0, =0x123456789`, `ldr w0, =sym`,
  `.ltorg`, `.pool`) with `$x`/`$d` mapping symbols, and the system
  instructions with every operand name GNU as knows — `dc`, `ic`, `at`,
  `tlbi`, `sys`/`sysl`, 1,619 `mrs`/`msr` registers and the PSTATE fields.
  Mapping symbols are an ELF convention, so a COFF or Mach-O object has none
  of them, and nothing raises a section's alignment for them there
- what the target selected *has*, on both ARM and AArch64: every
  architecture, CPU, unit and extension name GNU as knows, through `.arch`,
  `.cpu`, `.fpu` and `.arch_extension` or `-march=`, `-mcpu=` and `-mfpu=`,
  deciding which instructions assemble as well as what the object says. An
  `.arch armv4t` refuses a `clz`, an `.arch armv5te` an `sdiv`, and an
  `.arch armv8-a+nolse` an AArch64 `casp`, each naming what it would take.
  Each backend's own feature set and test are GNU as's -- any bit in common
  on ARM, every bit on AArch64 -- and each instruction's is the one its own
  row of binutils' tables carries; `src/arch/arm/cpu.rs` and
  `src/arch/aarch64/cpu.rs` say where every number comes from. With nothing
  selected the target is what the differential harnesses run the references
  as: `-march=armv7ve -mfpu=neon-vfpv4` on ARM, and ARMv9.5-A with every
  extension on AArch64
- the whole 680x0 family as GNU as knows it: the 68881/68882 FPU with float
  immediates in every size (`#1.5` in Motorola source, `#0r1.5` in GNU's), the
  68851 and on-chip MMUs, `cas2`, `callm`, `move16`, CPU32 and ColdFire,
  chosen by GNU as's CPU names (`-a 68040`, `.arch 5475`, `.arch 68000,68881`),
  with what the chosen CPU lacks refused by a message naming what it needs;
  the nine `@` suffixes an operand takes, which are `m68k-parse.h`'s
  `enum pic_relocation` -- the position-independent `@GOT`, `@PLT`, `@GOTPC`
  and `@PLTPC` and the thread-local `@TLSGD`, `@TLSLDM`, `@TLSLDO`, `@TLSIE`
  and `@TLSLE` -- each relocated by the width of the field the operand
  landed in, and `@TLSLDO` in a `.long`, which is the one data directive GNU
  as reads a suffix in; and a `:b`, `:s`, `:w` or `:l` displacement size,
  the byte one going in a brief extension word where the operand has one and
  widening to a word where it does not
- AVR as `avr-elf-as` assembles it: each core's own instruction set, chosen
  by family (`avr5`) or by device (`atmega328p`) with `-a` or `.arch`; the
  `lo8()`/`hi8()`/`pm()`/`gs()` modifiers in instructions and data; and
  objects prepared for linker relaxation, with every branch relocated, local
  labels in the relocations, `EF_AVR_LINKRELAX_PREPARED` in `e_flags`, and
  `.align` and `.org` in code recorded in `.avr.prop`
- the sections a reference writes into every object of its own accord, which
  say what a linker and a loader may do with it: ARM's `.ARM.attributes`,
  RISC-V's `.riscv.attributes` (the ISA string, and `.attribute` to change
  it), MIPS's `.reginfo` or `.MIPS.options` with the register masks and its
  `.MIPS.abiflags` (which `.module` changes), V850's `.note.renesas`,
  MSP430's `.MSP430.attributes`, and the `.gnu.attributes` that
  `.gnu_attribute` asks for, which on PowerPC is where the floating-point ABI
  is recorded
- MSP430 objects as GNU as writes them for a linker that relaxes code: every
  reference from code relocated, differences of code labels as
  `R_MSP430_SYM_DIFF` pairs (in the line table too), the `.MSP430.attributes`
  section and the `__crt0_*` references; and GNU as's polymorphic branches
  (`beq`, `bgt`, `jump`, …) in their long form
- RISC-V objects likewise: linker relaxation is on as GNU as has it, so an
  `R_RISCV_RELAX` marks every relocation a linker may rewrite — the two halves
  of an address, the `call` pair, the local-exec and descriptor thread-local
  sequences, and the GOT reference `la` and `lga` expand to, though not one
  the source wrote itself with `%got_pcrel_hi`, which is read here too — an
  `R_RISCV_ALIGN` hands an alignment in code to the linker with the padding it
  may delete, and a reference to a label in the same section is relocated
  rather than resolved, since the linker may shorten what lies between. So is
  a *difference* of two labels with something shortenable between them: an
  `R_RISCV_ADD`/`R_RISCV_SUB` pair at the field, chosen by its width, an
  `R_RISCV_SET_ULEB128`/`R_RISCV_SUB_ULEB128` pair in a `.uleb128`, and the
  same in the line table's address advances, the `.eh_frame` and
  `.debug_frame` advances (`R_RISCV_SET6` and its relatives) and the ranges a
  frame table and `.debug_aranges` record. A difference across code nothing
  can shorten is folded, as llvm-mc folds it. `.option relax`, `.option
  norelax` and `.option push`/`.option pop` turn all of that off and on again
  for the statements they cover
- diagnostics with source snippets that name the real limit, and assembly that
  continues past the first error

**Not yet**

- in NASM source: the multi-pass immediate-size optimizer for a value known
  only after layout, so `mov r64, len` where `len` is a label difference stays
  the sign-extending form rather than NASM's shorter 32-bit load (a constant or
  a symbol is optimized); x87, `enter`, far direct `jmp`/`call seg:off`, `[rip]`
  addressing (NASM uses `[rel]`), the `..gotpc`/`..gotoff`/`..tlsie` `wrt`
  targets and 16-bit object formats
- in CC-RL and CC-RH source: bit symbols, `$label`/`%label` gp- and
  ep-relative references, `STARTOF`/`SIZEOF`, and CC-RL's `HIGH`/`LOWW` of a
  relocatable label (all refused with the reason)
- in CC-RX source: `.FLOAT`/`.DOUBLE`, `.RVECTOR`, the `.LEN`/`.INSTR`/`.SUBSTR`
  string functions, `SIZEOF`/`TOPOF`, `__PID_REG`, big-endian sections, and
  bit length specifiers that ask for a longer form than the shortest (all
  refused with the reason)
- in PE/COFF objects: the CodeView directives that describe more than line
  and file information. `.cv_inline_site_id` and `.cv_inline_linetable`
  describe an inlined call site, whose rows belong to the caller's table at
  the call site's own position and are encoded as binary annotations;
  `.cv_def_range` says where a local variable lives; the `.cv_fpo_*` family
  describes an i386 frame in a `DEBUG_S_FRAMEDATA` subsection of its own.
  There is no `.debug$T` to write: no `.cv_*` directive makes a type record,
  so llvm-mc writes a type stream only for one a compiler hands it. Also
  unwind data for ARM64, whose `.seh_*` directives are a different set from
  x86-64's and whose records are a packed word where one will do and an
  extended one with epilogue scopes where it will not. All of those are
  refused with the reason. i386's `.safeseh` is refused for a reason of its
  own: GNU as, the reference for x86 PE objects here, has no such directive,
  so there is nothing to check an implementation of it against
- in Mach-O objects: armv7, whose relocations are a 32-bit object's with a
  set of its own above them (`ARM_RELOC_HALF` and its relatives) and whose
  symbols carry a mark saying which instruction set they are in; and a
  deployment version for a Darwin platform other than macOS, which llvm-mc
  reads from a triple such as `arm64-apple-ios17` and writes an
  `LC_BUILD_VERSION` for
- x86: of APX, the parts whose base instruction is itself missing — the
  user-mode MSR accesses (`urdmsr`, `uwrmsr`), the map-7 register forms of
  `rdmsr` and `wrmsrns`, AMX-TRANSPOSE's `t2rpntlvwz*` and AMX-MOVRS's
  `tileloaddrs*`, and VMX's `invept`/`invvpid`; the Xeon Phi 4FMAPS and
  4VNNIW register-group instructions, the
  `{disp8}`/`{disp32}`/`{load}`/`{store}`/`{nooptimize}` pseudo-prefixes,
  SGX, VMX, SVM, MPX and VIA PadLock
- DWARF: 64-bit DWARF, compressed debug sections, the `.cfi_*` directives
  beyond the common set (`.cfi_label`, `.cfi_val_encoded_addr`,
  `.cfi_inline_lsda`, `.cfi_fde_data` and llvm-mc's `.cfi_llvm_*`), and
  `.debug_macro`/`.debug_names`
- ARM: the divided Thumb syntax GNU as reads without `.syntax unified`
  (rsasm reads Thumb as unified syntax either way), which also means
  `.syntax divided` is accepted and ignored rather than changing how an
  operand is read, and that `-mimplicit-it=never` and `=thumb` warn about a
  conditional ARM instruction where GNU as would not have in divided source
- ARM vectors: the floating-point immediate of `vmov.f32 s0, #1.0` and
  `vmov.f64 d0, #0.5`, which needs a literal this assembler's GAS-dialect
  lexer does not read (`vcmp.f32 s0, 0` against the integer zero works);
  half-precision arithmetic (the conversions `vcvt.f16.f32`, `vcvtb` and
  `vcvtt` are there); and everything past ARMv7 and VFPv4 -- the ARMv8-A
  additions (`vrint`, `vcvta`/`vcvtn`/`vcvtp`/`vcvtm`, `vmaxnm`, `vsel`, the
  cryptographic and CRC instructions), ARMv8-M's security extension,
  ARMv8.1-M's low-overhead loops and MVE, the custom datapath extension and
  PACBTI, and the M-profile special registers of `vmrs`/`vmsr`
- AArch64: SME beyond `smstart`, `smstop` and `zero {za}` — the ZA array and
  its tiles, `zt0`, the multi-vector and strided register lists, predicates as
  counters and `psel`. That operand grammar is unlike anything else in the
  backend, and the few SVE2.2 and SVE2.3 forms written with a piece of it wait
  on it: `pext`, which indexes a predicate-as-counter, and the multi-vector
  `aesemc` and `aesdimc`, which name two register lists in one instruction.
  Beside it, three groups whose operand names belong in the generated system
  tables rather than written out by hand: FEAT_D128's `mrrs`, `msrr`, `sysp`
  and `tlbip`, which name a register pair and a 128-bit system register;
  `brb`'s two operations (BRBE); and GICv5's `gic`, `gicr` and `gsb`, whose
  operation names come from binutils' own tables as every other system
  instruction's do. `dfb` is GNU as's too, but only under `-march=armv8-r`,
  which nothing here targets.
  `movprfx` is assembled but its sequence is not checked, where llvm-mc
  refuses an instruction that does not use the prefixed register and GNU as
  warns
- PowerPC: POWER10's matrix-multiply accelerator (`xvi8ger4` and the other
  MMA instructions), POWER11's `xxaes*` and `xxgfmul128*`, decimal floating
  point, the quadword `lqarx`, `stqcx.`, `plq` and `pstq`, the `bctar`
  branches, and the privileged, hypervisor, cache-hint and synchronisation
  instructions POWER8–10 added (`stop`, `slbieg`, `hashst`, `mfdscr` and the
  like); the spellings only
  GNU as reads, so that nothing could check them (`@plt@ha`, `@sectoff`,
  `@sdarel`, `@got@dtprel@pcrel`, a halfword thread-local modifier on a
  prefixed instruction, and the thread-local modifiers in an eight-byte word
  in 32-bit code); and `@notoc`, which is a different relocation to each of
  the two references, and with it the POWER10 call
  `bl __tls_get_addr@notoc(sym@tlsgd)`
- MSP430: the large memory model (`-ml`), the interrupt-state `NOP`
  warnings and insertion, the silicon errata options, assembly-time
  relaxation (`-mQ`), and `.profiler`, `.refsym` and `.cpu`
- RISC-V: `.attribute arch` writes
  the ISA string as the source gave it, where GNU as reads it and writes back what it
  makes of it, so a string that leaves an implied extension out is not
  expanded, and neither it nor `.option arch` changes which instructions are
  accepted. `-mno-relax` has no counterpart on the command line, there being
  no per-target options there; `.option norelax` says the same thing for the
  statements it covers
- thread-local storage on the other targets: the symbols and sections are
  right everywhere, and every target with a thread-local model reads the
  access-model operands. SPARC's data operators — `%r_disp32()`, `%r_plt32()`
  and the thread-local `%r_tls_dtpoff32()`/`%r_tls_dtpoff64()`, which GNU as
  reads in `.word` and `.xword` — are not there either; llvm-mc, which is what the
  SPARC harnesses compare against, has none of them. ELF's access models are
  ELF's own, so an AArch64 thread-local operator in a Mach-O or PE object is
  refused as llvm-mc refuses it; Mach-O has a descriptor for each variable
  instead (see [Mach-O objects](#mach-o-objects)), and PE/COFF's own idea of
  the same thing is not there
- MIPS: the `.gnu.attributes` recording the floating-point ABI that GNU as
  writes and llvm-mc, the reference here, does not; the `.module` options
  that would change which instructions are accepted (the ISA names, the
  application-specific extensions), where the ones that only describe the
  floating-point unit are there; and the n32 ABI, since no target name
  selects it — `mips` and `mipsel` are o32, `mips64` and `mips64el` n64 —
  so of the two shapes of `$gp` prologue each is checked in one ABI rather
  than two. The position-independent operators are there, but not what
  `.abicalls` does to the *macros*: GNU as then reaches a symbol through the
  GOT for `la`, `j` and `jal`, in a sequence that differs by ABI, by whether
  the symbol is local and by whether a `.cprestore` has been seen, and `j`
  of a symbol stops being a jump at all, so rsasm refuses those three in
  position-independent code rather than assemble the direct form GNU ld will
  not put in a shared object. `-KPIC` has no rsasm spelling either, so
  `.abicalls` or `.option pic2` is the only way to ask for
  position-independent code. Five spellings GNU as takes are refused with
  the reason: `%neg` written as the outermost operator, which GNU as stops
  on with an internal error; a number under a composite operator, which it
  relocates against nothing; `%half` of a number, for which it writes the
  value in place of the instruction; `.gpdword` in an o32 object, where it
  writes two relocation entries for the one field and no linker reads them;
  and an addend wider than a signed 16-bit field on a `%got` of a local
  symbol, which GNU as puts in the field as a high half and refuses for a
  global symbol. `.tpreldword` is refused for the same kind of reason: GNU
  as 2.47 stops with an internal error on it, so no reference says what its
  eight bytes hold
- ARM: a second `.arch_extension`, or one beside an `.fpu`, takes each build
  attribute's largest value rather than merging feature bits as GNU as does;
  one on its own is exactly what GNU as writes, and
  `tools/tables/arm-attrs.py` checks every such pair. `-mfpu=` leaves the
  attributes `.fpu` would, where GNU as's option merges the unit into the
  CPU's own and so can keep a tag the directive replaces. Which instructions
  the selection has is a separate model and does merge the bits, so the two
  can disagree about a tag without disagreeing about an instruction
- ARM's floating-point unit: naming an architecture or a CPU does not take
  the unit away, because the default selection here has `neon-vfpv4` and
  nothing but a unit's own name replaces it. GNU as starts an
  `arm-none-eabi` target with no floating point, so where it refuses a
  `vadd.f32` under `-mcpu=arm7tdmi` until an `.fpu` or `-mfpu=` names a
  unit, rsasm assembles it. A unit that means none, `softfpa` or `softvfp`,
  takes it away in both
- ARM's M profile: `.arch armv7-m`, `armv6-m` and the ARMv8-M names select
  the feature set GNU as gives them, and a core with no ARM state starts in
  Thumb as GNU as's `-mcpu` does, but the instructions this backend has are
  an A-profile core's. What it accepts under an M-profile name is therefore
  wider than GNU as: the M-profile system registers (`ipsr`, `msp`), the
  narrower immediates and the width rules of the M-profile encodings are not
  modelled. `tools/tables/arm.py` says which architectures the instruction
  table claims
- ARM before Thumb-2: a 32-bit Thumb encoding is refused with the
  architecture that would take it named, where GNU as instead asks its
  encoder for a 16-bit one and refuses only what comes back wide anyway
  ("cannot honor width suffix"). The two differ over the few statements GNU
  as can rewrite to reach a narrow form, `adds r6, -1` becoming `subs r6, #1`
  on an ARMv4T, which rsasm refuses; `subs r6, 1` assembles. ARMv8-M
  Baseline's own rule, that a `mov` may take its 32-bit encoding although the
  architecture has no Thumb-2, is not modelled either, so a wide `mov` under
  `armv8-m.base` is refused where GNU as takes it
- AArch64: a mnemonic `insn.rs` encodes by hand carries the bits every row of
  its name needs, which is all a name alone can say, so one whose rows belong
  to different extensions -- `zero {za}`, which SME, SME2 and SME2.1 each
  have a form of -- is not gated at all; the forms of the generated table
  carry a set apiece and are. GNU as does not gate the `mrs`/`msr` register
  names or the PSTATE fields: its tables say which extension each belongs to
  and nothing ever turns that check on, so neither does rsasm
- 6502: the 65C02 and later instruction sets; in ca65 source, cheap local
  (`@loop`) and unnamed (`:`, `:-`) labels, `.proc`/`.scope`, `.struct`, and
  the `ZEROPAGE` segment's zero-page addressing for labels defined in it
- 8080: Intel's word operators (`AND`, `SHR`, `HIGH`, `MOD`), which AS does
  not read either
- 8051: the 8052's timer 2 names and the extended parts (80C320, 80C390,
  80251 and the rest); address spaces for `DATA`, `BIT`, `CODE` and the other
  defining words, which define plain values; and ASM51's controls
  (`$MOD51`, `$NOMOD51`), segments and relocatable output
- m68k: the ColdFire MAC and EMAC units; the suppressed registers `zpc`,
  `za0`-`za7` and `zd0`-`zd7`, and FPU coprocessor numbers other than 1
  (`.fopt id=`); vasm's `MACHINE`, `FPU` and `CHIP` directives (use `.arch`);
  vasm's sized `fbcc.w`, which GNU as does not take either (`fbcc` is 16
  bits, `fbcc.l` 32); CPU32's `tbl*` table lookups, for which no reference
  here has an encoding; and a size GNU as reads and throws away, which rsasm
  refuses: one on an operand whose width the instruction itself settles
  (`move.l #x:w,%d0`, `bra x:b`), one written in front of a suffix rather
  than after it (`x:w@GOT`, where `x@GOT:w` is the ordinary spelling), and
  one wider than the field the CPU has, which on a 68000 is a `:w` or `:l`
  displacement beside an index register. On ColdFire, an
  instruction as written that the core dropped is refused where GNU as
  substitutes one it kept (`addil #5,%a0@`, which GNU as writes as `addql`),
  since rsasm substitutes nothing
- Z80: the `DD CB d op,r` forms that also write a register, which vasm
  refuses; and in the GNU dialect, GNU as's `db`/`dw`/`ds` pseudo-ops (use
  `.byte`, `.word` and `.space`, or the 8-bit dialect)

- AVR: the `__gcc_isr` pseudo-instruction (`-mgcc-isr`), and Atmel's own
  AVRASM2 syntax, for which there is no free assembler to check against;
  `.arch` selects exactly the named core, where GNU as adds its instructions
  to those of an earlier one of the same machine

**Known wrong**

Anything that produces incorrect output rather than an error would be listed
here, separately. Nothing is at the moment.

Where the references themselves disagree, rsasm follows the one whose harness
checks the target (see [Verification](#verification)) and says so in the
backend. Five such choices are worth knowing about:

- **Which references are left to the linker.** A PC-relative reference to a
  global or weak symbol is relocated even when the symbol is in the same
  section, since the linker may bind the name elsewhere; a local one, or a
  local `.set` alias of a global one, is resolved. That is what both
  references do on nearly every target. The exceptions follow GNU as for
  x86, m68k, SuperH, RL78, AVR, MSP430 and RISC-V (a jump GNU as relaxes to a
  global symbol is resolved on x86; only weak symbols are left to the linker on
  m68k; nothing in the same section is on SuperH and RL78; and everything is
  on AVR, MSP430 and, once relaxation is on, RISC-V, whose linkers may delete
  code between a branch and its target). The distance as written still picks
  the instruction on RISC-V, as it does in both references, and is no longer
  range-checked, since it is not the distance the linker will write; the field
  is left empty, as llvm-mc leaves it and as every relocated field here is,
  where GNU as fills it in for a reader's benefit. Such a reference names the
  label rather than its section plus an offset, as both references name it:
  a linker that deletes bytes moves the label and adjusts the symbol, and
  would leave an offset into the section behind. On ARM GNU as is followed for
  whole objects: a `bl` to a local label is resolved, and made a `blx` where
  the label is a Thumb function, where llvm-mc relocates every `bl`.
- **Default section alignment.** Sections start with the alignment the
  reference gives them: 16 for MIPS `.text`, `.data` and `.bss` (llvm-mc; GNU
  as aligns only `.text`, to 4), 4 for `.text` on PowerPC and SPARC and for
  every executable section on AArch64 (llvm-mc; GNU as gives 1, or aligns
  once an instruction is assembled), on ARM what GNU as gives (a section is
  aligned by the first instruction in it, to 4 for ARM and 2 for Thumb), 2 or 4 for RISC-V `.text` depending on
  compressed instructions, 4 for m68k `.text`, `.data` and `.bss`, and 1
  otherwise, including on x86, where GNU as is followed and llvm-mc's `.text`
  is 4.
- **The build attributes.** The section a reference writes of its own accord
  is the one that reference's, and where they differ so does rsasm: ARM's
  `.ARM.attributes` is GNU as's, which llvm-mc does not write at all unless
  the source asks for one, and MIPS's `.reginfo` and `.MIPS.abiflags` are
  llvm-mc's: GNU as writes the same `.MIPS.abiflags`, a `.reginfo` without
  `SHF_ALLOC`, and beside them a `.pdr` and a `.gnu.attributes` that rsasm
  writes neither of. The two also work the register masks out differently
  where the floating-point file is 32 bits wide and a double therefore fills
  a register pair: llvm-mc counts the pair for each operand that holds a
  double, and GNU as counts it for every floating-point operand of any
  instruction with a double-precision form, so they part company on
  `cvt.d.s` and the other mixed conversions. rsasm counts what llvm-mc
  counts. RISC-V's `.riscv.attributes` is the same in both.
  `tools/mc-diff` leaves `.ARM.attributes` out of its comparison for that
  reason, and `tools/xas-diff` compares it.
- **Common blocks, and symbols only a directive names.** On every ELF
  target GNU as is followed, since it is the assembler a GNU toolchain runs
  and aligning less than it does can misalign what the linker places. A
  `.comm` or `.tls_common` that names no alignment (or 0) is aligned to its
  size rounded up to a power of two, at most 16, where llvm-mc aligns it to
  1. `.lcomm`, and a `.comm` of a symbol `.local` named first, reserve the
  object in `.bss`, as both references do; `.lcomm` aligns it to 8, 4 or 2 by
  its size (to 8 whatever the size on PowerPC, and not at all on AVR and
  MSP430), where llvm-mc packs `.bss`. A name only `.type`, `.size`,
  `.globl`, `.local` or a visibility mentions is written as a global
  undefined symbol, and one only `.weak` mentions is left out, even after a
  `.globl`, which leaves it weak; llvm-mc keeps the weak one, leaves out the
  one only `.size` names, makes the `.local` one local, and refuses `.globl`
  after `.weak`. The NASM dialect keeps an `extern` nothing refers to out of
  the object, as NASM does. Mach-O and COFF objects follow llvm-mc here,
  as they do elsewhere: a common block's alignment is Darwin's power of two
  in the one and makes the block larger in the other, and `.lcomm` packs
  `.bss` in both, where the mingw GNU as aligns by size as on ELF.
- **ARM's thread-local fields.** GNU as is followed. Against a variable
  defined in the object, `x(TLSLDO) + n` leaves the variable's offset in the
  field as well as `n`, which GNU ld then counts twice, and `x(TLSLDM) + n`
  leaves zero where `n` is that offset; a Thumb `bl x(tlscall)` has a
  displacement of zero; and `.tlsdescseq` in Thumb code writes
  `R_ARM_THM_TLS_DESCSEQ`. llvm-mc writes `n` alone, the usual -4, and
  `R_ARM_TLS_DESCSEQ`.
- **m68k floating-point immediates.** Both references write a single or
  double precision `#1.5` the same way. An extended-precision one GNU as 2.47
  writes without the 16 zero bits of the 68881 format — its own `.extend`
  directive and disassembler have them — and a packed-decimal one it refuses;
  vasm writes both correctly, from a C `double`, and so does rsasm. The m68k
  backend follows GNU as otherwise.

## Usage

```
rsasm [options] <input.s>...

  -o <file>          write output to <file> (default: a.out)
  -a, --arch <name>  target architecture (default: the host, if supported),
                     or a target triple: `x86_64-apple-macos` also picks
                     Mach-O output, `x86_64-pc-windows-msvc` PE/COFF. A
                     macOS version in a Darwin triple is the object's
                     deployment target (`x86_64-apple-macos10.6`), which is
                     what asks for a compact unwind table on x86-64
  -f, --format <fmt> output format: elf (default), elf32, elf64, coff,
                     win64, win32, macho, bin or ihex
  -s, --syntax <s>   initial operand syntax: att (default) or intel
  -d, --dialect <d>  source dialect: gas, nasm, motorola, renesas (CA78K0),
                     ccrl (Renesas CC-RL), ccrh (Renesas CC-RH),
                     ccrx (Renesas CC-RX) or 8bit (6502, Z80, 8080, 8051)
                     (default: the architecture's usual one)
      --march=<name> ARM and AArch64: the architecture to assemble for, which
                     decides which instructions are accepted -- `armv5te`,
                     `armv9.5-a+sve2+nolse` -- spelled as GNU as spells it,
                     `-march=<name>`, as well
      --mcpu=<name>  the same for a named CPU, which wins over `--march`
      --mfpu=<name>  ARM: the floating-point and SIMD unit, `neon-vfpv4`
      --mimplicit-it=<m>
                     ARM: when a conditional Thumb instruction with no `it`
                     block of its own gets one made up for it -- never, arm
                     (default), thumb or always -- spelled as GNU as spells
                     it, `-mimplicit-it=<m>`, as well
  -I <dir>           add <dir> to the .include search path
  -D <sym>[=<val>]   define <sym> before assembling
      --base <addr>  base address for `bin` and `ihex` output
      --hex          print the output as hex instead of writing a file
  -g                 describe the assembly source in DWARF line information
      --gdwarf-<n>   the same, as DWARF version <n> (2 to 5); the version
                     also applies to `.loc` source
      --list-arch    list the architectures this build supports

linking (`--link`, x86-64 and AArch64 ELF or PE32+ only):
      --link         link the object into a runnable program instead of
                     writing it, and write that to <file>
  -e, --entry <sym>  entry symbol (default: the target's, `_start`)
  -l <name>          link against lib<name>; `-l:<file>` names a file in the
                     search path, which is how `crt1.o` is reached
  -L <dir>           add <dir> to the library search path
      --dynamic-linker <path>
                     record <path> as the program interpreter
      -shared        link a shared library
      -pie           link a position-independent executable
```

Architectures are cargo features, all on by default — `x86`, `aarch64`, `arm`,
`riscv`, `powerpc`, `mips`, `sparc`, `retro` (the Z80, 6502, 8080 and 8051),
`m68k`, `superh`, `rx`, `rl78`, `v850`, `k78`, `avr` and `msp430` — and so is
`link`, described under "Linking" below:

```console
$ cargo build --no-default-features --features x86,aarch64
```

### As a library

```rust
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rsasm::section::SectionId;
use rsasm::{arch, output};

let options = Options::new()
    .with_dialect(Dialect::Gas)
    .with_include_path("include");

let mut asm = Assembler::new(arch::lookup("x86-64").unwrap(), options);
asm.assemble_str("example.s", "movq %rbx, %rax\nret\n");
if !asm.finish() || asm.diags().has_errors() {
    eprint!("{}", asm.diags().render(asm.source_map(), false));
} else {
    let text = asm.section_bytes(SectionId(0));
    let object = output::elf::build(&asm).unwrap();
}
```

Only a small part of the crate is API: `Assembler` and the methods that drive
it, `Options` and its `with_*` builders, `output::Format` and the writers'
`build`, `arch::lookup` with the `Architecture` trait, `section::SectionId`,
the diagnostics types, `lexer::Dialect`, and — with the `link` feature —
`link::link` and what it takes. Everything else — the opcode
tables, the operand parsers, the expression arena, the macro engine, the
layout — is an implementation detail and is not documented on docs.rs; see
"What is public API" there for the exact list. `Options` and the other types
that will keep growing are `#[non_exhaustive]`, so build them with their
constructors rather than with a struct literal.

## Dialects

Source syntax is chosen with `-d`, or defaults to what each architecture's
source is normally written in.

| Dialect | Looks like | Default for |
|---|---|---|
| `gas` | `.byte 1`, `# comment`, `0x10` | most targets |
| `motorola` | `dc.b 1`, `; comment`, `$10`, `%1010` | m68k |
| `renesas` | `DB 'A',1`, `; comment`, `10H` | 78K0 |
| `ccrl` | `.DB "A",1`, `$IF`, `0x10` or `10H` (Renesas CC-RL) | — |
| `ccrh` | `.dw #label`, `$IF`, `0x10` (Renesas CC-RH) | — |
| `ccrx` | `.SECTION P,CODE`, `.LWORD 10H`, `#1:8` (Renesas CC-RX) | — |
| `8bit` | `lda #$12`, `ld a,(ix+5)`, `MVI A,12H`, `SETB P1.3`, `DB 1`, `; comment` | 6502, Z80, 8080, 8051 |
| `nasm` | `db 1`, `; comment`, `mov eax, [rel x]`, `%macro`, `0FFh` | — |

```console
$ cat intena.s
        move.w  #$7fff,$DFF096          ; disable all Amiga interrupts
$ rsasm -a m68k -f bin --hex intena.s
33 fc 7f ff 00 df f0 96
```

Motorola covers vasm, Devpac and ASM-One source and was checked against both
vasm and GNU as `--mri`. Four rules in it catch people out:

- **A word in the first column is a label**, with or without a colon, so
  instructions have to be indented. `rts` written in column 0 assembles to no
  code at all — in both reference assemblers, not just here — and so does a
  dotted directive: `.section` in column 0 is a label, and the word after it
  is what the line is read as.
- **`.section` is GNU as's directive, `section` the Motorola one.** Neither
  reference reads a dotted directive at all, so the dot can only be GNU as's
  spelling: `section name[,type]` names a code, data or bss section, and
  `.section .tbss,"awT",@nobits` takes GNU as's flag string and type. A `#`
  where a line begins is a comment, as `*` is, which is what GNU as `--mri`
  reads; vasm calls it an error.
- **Word and long data, and instructions, are aligned to an even address.**
  vasm on its own defaults leaves a `dc.w` after a `dc.b` at an odd address;
  Devpac, GNU as and vasm's `-devpac` mode align it, and a 68000 faults on the
  alternative. GNU as's own m68k syntax aligns nothing, so this is a property of
  the dialect.
- **Instructions are assembled as written.** vasm's default optimizer turns
  `move.l #1,d0` into `moveq #1,d0`; rsasm, like GNU as, only chooses the
  shortest encoding of the instruction you wrote.

### The 8-bit dialect

`8bit` reads the source people have for the 6502, the Z80, the 8080 and the
8051: ca65's for the 6502, Zilog's as GNU as and vasm read it, and Intel's as
AS reads it. Their spellings are one language — `$12`, `12H`, `%1010` and
`0x12` numbers, `$` and `*` for the location counter, `<`, `>` and `^` for
the bytes of an address, `DB`/`DEFB`/`.byte`, `DW`/`DEFW`/`.word`,
`DS`/`DEFS`/`.res`, `EQU`, `=`, `DEFL`/`SET`, `IF`/`ENDIF`, `MACRO`/`ENDM` or
`.macro`/`.endmacro`, ca65's `.segment` — and where the references disagree,
rsasm picks one and says so:

```console
$ cat hello.asm
bdos    equ 5
        org 100h                ; a CP/M program
start:  ld de,msg
        ld c,9
        call bdos
        ret
msg     db 'Hello$'
$ rsasm -a z80 -f bin --hex hello.asm
11 09 01 0e 09 cd 05 00 c9 48 65 6c 6c 6f 24
```

- **A word in the first column is a label, unless it is an instruction or a
  directive.** vasm and AS take any first-column word as a label; ca65 and
  GNU as want a colon and assemble `rts` written there. Both kinds of source
  work, except a colonless label spelled like a mnemonic, or a macro called
  from the first column.
- **`ORG` says where code is loaded.** The first `ORG` in a section is its
  address in the image, not padding: `ORG 100H` does not put 256 zeros in
  front of a CP/M program. A later `ORG` pads up to its address, as vasm and
  AS do; ca65 does not pad.
- **Zero page is chosen as ca65 chooses it:** for a constant or `ORG`-placed
  label known before its use, and for `<addr`; a forward reference is
  absolute, and `z:`/`a:` override either way. vasm, a multi-pass assembler,
  picks zero page for forward references too.
- **The location counter in a data list is each item's address**: `.word
  *, *` is two different values, as in ca65, vasm and GNU as. AS keeps the
  statement's address.
- A comparison is 1 when true, as in ca65 and AS; GNU as and vasm give -1.
  The operators have C's precedence, where ca65 binds `&` as tightly as `*`.

The 8051 adds what its source needs, with AS as the reference and SDCC's
sdas8051 as the second:

```console
$ cat blink.asm
LED     BIT     P1.0
        ORG     30H
MAIN:   MOV     TMOD,#01H
LOOP:   CPL     LED
        ACALL   DELAY
        SJMP    LOOP
DELAY:  DJNZ    R7,DELAY
        RET
$ rsasm -a 8051 -f ihex --hex blink.asm
:0C003000758901B290113980FADFFE22C0
:00000001FF
```

- **A bit is written `byte.bit`,** `P1.3` or `20H.5`, and the `.` splits the
  whole operand as AS splits it, so `20H+1.3` is bit 3 of 21H. Only 20H to
  2FH and the registers at a multiple of 8 have bits; AS warns about other
  bytes, or for 30H to 3FH says nothing, and assembles a bit of some other
  byte, where rsasm refuses them.
- **The register and bit names are predefined,** as AS's `stddef51.inc`
  defines them for the 8051, in upper and lower case, with its `USING` and
  the `AR0`–`AR7` names; a label or `EQU` may take one over. `BIT`, `DATA`,
  `IDATA`, `XDATA` and `CODE`, and AS's `SFR` and `SFRB`, define a name.
- **`CY` is the carry flag wherever `C` could stand,** as in AS: `CPL CY` is
  the one-byte `CPL C`, and `JB CY,$` tests bit D7H.
- **`JMP` and `CALL` become the shortest jump that reaches,** `SJMP`, then
  `AJMP` or `ACALL`, then `LJMP` or `LCALL`, with every size picked again on
  each pass, as AS picks them.
- **`AJMP` and `ACALL` reach the 2 KiB block of the address after them,**
  which is where the CPU takes the block from. AS and sdas8051 test the
  instruction's own address, and differ from rsasm, and the CPU, only for
  one in the last two bytes of a block.
- **`DW` is low byte first,** as in AS; sdas8051's `.dw` is high byte first.
  A 16-bit instruction operand, `LJMP 1234H` or `MOV DPTR,#1234H`, is high
  byte first in every assembler, as the CPU reads it.

### Renesas CC-RL, CC-RH and CC-RX

`ccrl`, `ccrh` and `ccrx` read source written for the assemblers of Renesas's
RL78, RH850 and RX compiler packages. GNU as stays the default for those
targets, because
GNU-syntax source would not always be refused in the vendor dialects — it
would sometimes mean something else — so the dialect has to be asked for:

```console
$ cat start.asm
        .CSEG   TEXT
_start: MOVW    SP, #LOWW(0xFFE00)
        MOV     [HL], #0                ; CC-RL's shorthand for [HL+0]
        BR      !!_main
$ rsasm -a rl78 -d ccrl -o start.o start.asm
```

What is covered, from the *CC-RL Compiler User's Manual* (R20UT3123EJ0115)
and the *CC-RH Compiler User's Manual* (R20UT3516EJ0113):

- comments, both number notations (CC-RL), escapes in strings, `@` in symbols
- each assembler's own operator precedence, 32-bit `>>`, and the `HIGH`,
  `LOW`, `HIGHW`, `LOWW` and `HIGHW1` separators
- `.CSEG`/`.DSEG`/`.SECTION` with their relocation attributes, default names
  and alignments, `.ORG`, `.OFFSET`, `.ALIGN`, the `.DB` family, `.EQU` and a
  redefinable `.SET`, `.PUBLIC`/`.EXTERN`/`.WEAK`
- the `$IF`/`$IFDEF`/`$ELSEIFN` family, `$INCLUDE`, `$BINCLUDE`, and macros with
  named parameters, `.LOCAL`, `?`/`~` concatenation, `.REPT` and `.IRP`
- CC-RL's `[DE]`/`[HL]` zero-displacement shorthand
- CC-RH's instruction expansions — `mov 0x10, r10` is a `movea`, `add
  0x12345, r10` a load into `r1` — its `#label`/`!label` references, condition
  suffixes (`setfgt`, `cmovz`, `cmpfeq.s`), `jr22`/`ld23.w`-style width
  spellings, `push`/`pushm`, and byte-sized `prepare`/`dispose` frames

CC-RX has a directive set of its own, covered from the *CC-RX Compiler User's
Manual* (R20UT3248EJ0115): `B`/`O`/`H` number suffixes, names with `$` and
`.`, `$` as the location symbol, its operator precedence, `.SECTION` with
`CODE`/`ROMDATA`/`DATA` and `ALIGN=`, `.ORG` and `.OFFSET` with the NOP code
or `FILL` as padding, `.ALIGN`, `.BLKB` to `.BLKD`, `.BYTE`/`.WORD`/`.LWORD`,
`.EQU`, `.GLB`/`.WEAK`, `.INCLUDE`, `.END`, `.IF`/`.ELIF`, `.DEFINE`, `?:`
temporary labels, the `__PID_R0`-`__PID_R15` register names, and macros with
`..MACPARA`, `.MREPEAT`/`..MACREP`, `.LOCAL` and `@` concatenation.

```console
$ cat reset.src
        .SECTION P,CODE
        .GLB    _start
_start: MOV.L   #0FFH:8, R1
        ADD     400[R1], R2
        BRA     ?+
        NOP
?:      RTS
        .END
$ rsasm -a rx -d ccrx -o reset.o reset.src
```

CC-RX honours a bit length specifier such as `#1:8` even where a shorter form
fits; GNU as ignores it and rsasm, like GNU as, assembles the shortest form.
So a specifier is accepted where it names the width of that form, and refused
where CC-RX would have produced something else.

No Renesas assembler can be run here, so rsasm's reading of the manuals is
checked the only way it can be: each case in `tools/xas-diff/rl78-ccrl-pairs.txt`,
`rh850-ccrh-pairs.txt` and `rx-ccrx-pairs.txt` pairs vendor source with the
GNU-syntax program it means, and `tools/xas-diff/run.sh` requires rsasm's
bytes for the first to equal GNU as's for the second. Placement is the
linker's: an `AT` attribute or `.ORG` names the section (CC-RL, CC-RH) or pads
it (CC-RX) as the manual says, but the start address is not recorded.

### NASM

`-d nasm` reads source written for NASM, the flat-binary and ELF assembler most
x86 hand-written code targets. The whole language people reach for is there:

- a preprocessor run a line at a time as NASM's is — `%define`/`%xdefine`/
  `%assign`/`%undef`, `%macro` with parameter ranges, defaults, greedy `+`
  params, `%0`, `%rotate`, `%%` labels and `%00` label capture, `%rep`/
  `%exitrep`, the `%if`/`%elif`/`%else` family (`%ifdef`, `%ifmacro`, `%ifidn`,
  `%ifnum`, `%ifstr`, `%ifctx` …), `%include`, `%strlen`/`%substr`/`%defstr`,
  `%push`/`%pop` contexts with `%$` locals, and `%error`/`%warning`
- `db`/`dw`/`dd`/`dq`/`dt` with single-, double- and backquoted strings, the
  `resb` family, `times n <stmt>` (including `times 510-($-$$) db 0`), `incbin`,
  `equ`, `struc`/`endstruc`/`istruc`/`at`/`iend`, `align`/`alignb`, and
  `absolute`
- `section`/`segment` with attributes (`progbits`, `nobits`, `alloc`, `exec`,
  `write`, `align=`), `bits 16/32/64`, `org`, `global`/`extern`/`common`/
  `static` with `:function`/`:data` and sizes, `default rel`/`abs`, `$`/`$$`
  and `.local`/`..@` labels
- NASM's operand syntax: the `byte`/`word`/`dword`/`qword` size keywords with
  no `ptr`, `short` and `near` on a branch, `[rel x]` and `[abs x]`, segment
  overrides `[es:di]`, the moffs accumulator forms, and 8086 16-bit
  addressing; and the `wrt ..plt`, `wrt ..got`, `wrt ..sym` and
  `wrt ..gotoff` ELF relocations

Much of what looks like NASM directive syntax — `section`, `global`, `struc`,
`align` — is macros in NASM's standard macro set wrapping a bracketed
primitive, `[section .data]`; rsasm defines the same macros, so `__SECT__` and
the rest behave as they do there.

```console
$ cat boot.asm
        org     0x7c00
        bits    16
start:  mov     ax, 0x1234
        jmp     start
        times   510-($-$$) db 0
        dw      0xaa55
$ rsasm -d nasm -f bin -o boot.bin boot.asm   # a 512-byte boot sector
```

`tools/nasm-diff/run.sh` assembles a corpus of whole programs with rsasm
`-d nasm` and with NASM 2.16.03 (built by `tools/oracles/build.sh`), and
compares the flat binaries byte for byte and the ELF objects section by
section, relocations and global symbols included. Local symbols are not
compared in ELF objects: NASM writes every label into the symbol table, where
rsasm, like GNU as, keeps them to itself, and a linker never sees the
difference. `-f win64` and `-f win32` objects are compared whole, as
`tools/coff-diff/canon.sh` prints them; see [PE/COFF](#pecoff). 444 of 444
match.

## Multi-architecture files

`.arch <name>` switches the target for everything after it, so one file can
hold, say, a boot stub for one CPU and the code it loads for another:

```console
$ cat two.s
        movl    $0x10000, %esp          # x86: `#` starts a comment
        .arch   m68k
        movew   #0x2700, %sr            | m68k: `#` is an immediate, `|` a comment
        .arch   sh
        mov     #1, r0                  ! SuperH: `!` is the comment
$ rsasm -a i386 -f bin --hex two.s
bc 00 00 01 00 46 fc 27 00 e0 01
```

- **Source is read as the target it is for.** The statement after an `.arch`
  is lexed by the new target's rules — its comment characters, and number
  spellings such as RL78's `10H` — whether the switch was in the file itself,
  in a macro expansion or in an included file. The rest of the `.arch` line,
  a trailing comment say, still belongs to the old target. The dialect (`-d`)
  does not change.
- **A macro body is read where it is expanded.** Only where it ends is found by
  the rules in force at `.macro`; the body is kept as text and lexed by the
  rules in force at each expansion, like an included file.
- **An `.arch` that is not assembled does nothing,** whether it is in a false
  conditional or in a macro that is never called.
- **Code keeps its target.** Byte order, branch displacements resolved at the
  end, and the no-ops that pad an alignment are those of the target the code
  was written for, not the one active at the end of the file.
- **The object is for the starting target.** The ELF class, machine and byte
  order are those of `-a`. Code for a different machine, byte order or word
  size can be in it, but cannot be relocated: a reference in that code has to
  resolve within the file.
  Switching to another CPU of the object's own machine (`.arch sh4` in an `sh`
  file) carries over what the ELF header records about the code so far.

Every case in `tools/multiarch-diff/programs.txt` is checked the only way it
can be: the file is split at its `.arch` lines, each part is assembled by its
own target's reference, and rsasm has to produce the concatenation.

## Debug information

`.file N "name"` (with a directory and `md5` in DWARF 5) and `.loc` with all of
its options, `view` included, write `.debug_line` and `.debug_line_str`; the
`.cfi_*` directives write `.eh_frame` or `.debug_frame`, as `.cfi_sections`
says. Where the source brings no `.debug_info`, the compilation unit an
assembler makes up for the table is written too. `-g` (or `--gdwarf-<n>`)
instead describes the assembly source: a row for each instruction, at its
line, and a unit naming the file.

The two references agree on the formats and disagree on nearly everything
inside them, so each target follows the one that checks its encodings: GNU as
for x86, m68k, SuperH, RX, RL78, V850, AVR and MSP430, and llvm-mc for the rest. That
decides, among other things, the default version (3 for GNU as, 4 for
llvm-mc, 5 for either once a `.file 0` appears), how a path splits into a
directory, whether a column carries over to the next `.loc`, which directives
end a pending `.loc`, how CIEs are shared, and how padding and relocations are
written. Each backend supplies its DWARF register numbers and names, return
address column, alignment factors, initial instructions and FDE encoding; RX,
RL78, V850 and MSP430, whose GNU as has no CFI, refuse `.cfi_*` as it does. For `-g`,
GNU as places an instruction from a macro on the line that called it, one
from `.rept` or `.irp` on its line in the block, and one from an included file
on its line there; llvm-mc puts every instruction in the main file at the
outermost line that expanded it, and describes each label as well.

Which of the two that is depends on the object format as well, since for some
formats only one of them writes anything: GNU as for mingw writes x86's PE
objects and llvm-mc writes ARM64's, and llvm-mc alone writes Mach-O. So
x86-64's DWARF is GNU as's in an ELF or a PE object and llvm-mc's in a Mach-O
one.

The sections go wherever the format puts them, and so does the work a
relocation would otherwise do. ELF and PE/COFF keep DWARF's own names;
PE/COFF gives each section the characteristics the mingw assembler gives it,
`IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_DISCARDABLE |
IMAGE_SCN_MEM_READ`, and writes an offset from one of them into another as
`IMAGE_REL_*_SECREL`, which is the relocation COFF has for an offset within a
section. A Mach-O object gathers them into a `__DWARF` segment
(`__debug_line`, `__debug_line_str`, `__debug_info`, `__debug_abbrev`,
`__debug_str`, `__debug_aranges`, `__debug_ranges`, `__debug_rnglists` and
`__debug_frame`, each marked `S_ATTR_DEBUG`) and puts the frame table in
`__TEXT,__eh_frame`, with the compact unwind table beside it on arm64 (see
[Compact unwind](#compact-unwind)), and needs far fewer relocations than
either format: every section in the object already has an address there, so an
offset into another debugging section is a number, and on x86-64 an FDE's
address and its language-specific data area are distances the assembler works
out. What is left is the addresses of code, which name the section they are in
rather than the function, since a debugger reads a debugging section expecting
the values in it to be filled in already; on arm64, where every relocation
has to name a symbol, an FDE's address is instead the difference of the
function and the field, a `SUBTRACTOR` pair. The CIE moves with the format
too: GNU as numbers the return address column 32 in a PE object on x86-64
rather than the psABI's 16, llvm-mc gives an FDE in a PE object a plain
pointer where ELF has a four-byte PC-relative one and a pointer-sized
PC-relative one in a Mach-O object, Darwin reaches the personality routine
through the GOT whatever the encoding byte says, and an AArch64 CIE's data
alignment is -8 there against ELF's -4, since Darwin alone gives
`CalleeSaveStackSlotSize` the width a saved register really has.

A target whose linker deletes instructions cannot write a distance as a
number. On MSP430 every address advance in the line table is a 16-bit field
the linker fills in, as GNU as writes them; on RISC-V only the advances whose
span the linker may shorten are, and the frame tables go the same way — an
advance left to the linker keeps its `DW_CFA_advance_loc` opcode and has the
bits beside it set by an `R_RISCV_SET6`, `SET8`, `SET16` or `SET32`, and the
range an FDE or `.debug_aranges` records is an `R_RISCV_ADD`/`R_RISCV_SUB`
pair.

Four differences remain:

- GNU as gives a `view -0` row an address of its own wherever its frag
  obstack happened to start a new chunk, which depends on the host's memory
  allocation; rsasm does so only where the row is at the address of the one
  before, which is when the view count needs it.
- On RL78, GNU as leaves every distance in the line table to the linker as a
  stack of relocation operations; rsasm writes the distances, which are final
  since it lays out the section itself. On AVR GNU as writes the distances
  too, and adds an `R_AVR_DIFF*` relocation to each, for linker relaxation;
  rsasm writes the distances alone.
- For `-g` on llvm-mc's targets, llvm-mc numbers the last statement of an
  included file against the file that included it, reading past that file's
  buffer; rsasm gives its line in the included file.
- `-g` writes DWARF into a PE object. GNU as for mingw writes STABS there
  unless `--gdwarf-<n>` asks for DWARF, and rsasm has no STABS to write.

The producer named in the unit is `rsasm` and its version, or the value of
`DEBUG_PRODUCER`, which llvm-mc also reads.

A Windows object can carry CodeView instead, which is the format MSVC's
toolchain reads and not a dialect of DWARF: the `.cv_*` directives write
subsections of a `.debug$S` stream, with a string table and file checksums of
their own. See [PE/COFF](#pecoff).

## PE/COFF

`-f coff` writes a Windows object file for the target: an AMD64 object for
`x86-64`, I386 for `i386` and ARM64 for `aarch64`. `-f win64` and `-f win32`
are NASM's names for the same thing, and choose `x86-64` or `i386` when `-a`
does not:

```console
$ rsasm -f win64 -o hello.obj hello.s     # then link.exe, lld-link or mingw ld
```

What the source can say:

- sections with `.section name,"flags"`, in llvm-mc's reading of the flag
  letters (`x`, `r`, `d`, `w`, `b`, `n`, `s`, `y`, `i`, `D`), `$`-grouped
  names such as `.text$mn` and `.CRT$XCU`, and COMDATs: `.section
  name,"flags",<selection>,<symbol>` with `discard`, `one_only`, `same_size`,
  `same_contents`, `associative`, `largest` or `newest`, and `.linkonce`.
  Sections of one name told apart by their COMDAT symbol stay apart, as a
  compiler's one `.rdata` per folded constant needs
- symbols: `.def`/`.scl`/`.type`/`.endef`, `.weak` as a weak external,
  `.comm` (with its alignment as a power of two) and `.lcomm` (into `.bss`),
  absolute and `.set` symbols, `.file`, and names with `@` in them, such as
  MSVC's mangled ones
- relocations for all three machines, with COFF's convention of keeping the
  addend in the relocated bytes: `IMAGE_REL_AMD64_ADDR64`, `ADDR32`,
  `ADDR32NB`, `REL32`, `SECTION` and `SECREL`; `IMAGE_REL_I386_DIR32`,
  `DIR32NB`, `REL32`, `SECTION` and `SECREL`; and `IMAGE_REL_ARM64_BRANCH26`,
  `BRANCH19`, `BRANCH14`, `PAGEBASE_REL21`, `REL21`, `PAGEOFFSET_12A`,
  `PAGEOFFSET_12L`, `ADDR64`, `ADDR32`, `ADDR32NB`, `REL32`, `SECTION` and
  `SECREL`; `.rva`, `@IMGREL` and NASM's `wrt ..imagebase` for image-relative
  addresses, `.secrel32` and `@SECREL32`, and `.secidx`
- x86-64 unwind data: `.seh_proc`, `.seh_pushreg`, `.seh_stackalloc`,
  `.seh_setframe`, `.seh_savereg`, `.seh_savexmm`, `.seh_pushframe`,
  `.seh_handler`, `.seh_handlerdata`, `.seh_endprologue` and `.seh_endproc`
  write `.xdata` and `.pdata`, counting the prologue from the final lengths
  of its instructions. A function outside `.text` gets a pair of its own under
  the same COFF name, and one in a COMDAT section an associative COMDAT keyed
  on that section's symbol, so that the linker keeps or drops the unwind data
  with the copy of the code it describes. Those sections are numbered after
  every section that is not associative, since MSVC's linker cannot read a
  forward reference
- DWARF: `.file` and `.loc` write the `.debug_*` sections, `.cfi_*` writes
  `.eh_frame` or `.debug_frame`, and `-g` describes the source; see
  [Debug information](#debug-information). A function can carry both
  descriptions at once, `.seh_*` for the Windows unwinder and `.cfi_*` for a
  DWARF one, as it can in the mingw assembler
- CodeView line and file information, which is what an MSVC-targeted
  toolchain reads rather than DWARF: `.cv_file`, `.cv_func_id`, `.cv_loc`,
  `.cv_linetable`, `.cv_filechecksums`, `.cv_filechecksumoffset`,
  `.cv_string` and `.cv_stringtable` write the `DEBUG_S_LINES`,
  `DEBUG_S_FILECHKSMS` and `DEBUG_S_STRINGTABLE` subsections of a `.debug$S`
  stream. Each goes where its directive stands, in whatever section is
  current, since that is the only thing that puts the subsections of a stream
  in order; a line subsection names the function it describes with an
  `IMAGE_REL_*_SECREL` and an `IMAGE_REL_*_SECTION` against its start and
  holds each row's offset as a number the layout works out. A `.cv_loc` makes
  its row where it stands rather than at the next instruction, as `.loc`
  does, so two in a row make two rows at one address. llvm-mc is the only
  reference here that writes any of this, since GNU as for mingw has no
  `.cv_*` directive at all; see `src/codeview.rs`

Backends choose relocations as ELF numbers, the one numbering all of them
share, and name in a `reloc::RelocClass` what a number cannot say;
`src/output/coff/reloc.rs` translates both into COFF's numbering, and says
where each COFF relocation measures its PC from, which is what the addend in
the field has to make up for. Nothing in a backend knows COFF exists, and ELF
output does not go through the translation. `@IMGREL`, `.rva`, `.secrel32`
and `.secidx` name something no psABI has a number for, so they travel as
classes of their own; they are refused outside COFF output.

llvm-mc is the reference: it writes COFF for all three machines, and it is
the assembler of the LLVM Windows toolchains. GNU as for mingw agrees with it
on relocations — which ones, where, of what type, against what, and what the
field holds — and on almost nothing else, so where the two differ rsasm
follows llvm-mc:

- `.text`, `.data` and `.bss` are always present and four-byte aligned, and a
  section is not padded at its end; GNU as aligns them to 16 and pads.
- Section symbols carry a checksum of the section, and there is no `.file`
  symbol unless the source has a `.file`; GNU as writes no checksum and a
  `.file` named `fake`.
- A relocation against a local label names the label, which is in the symbol
  table; GNU as names its section and puts the label's offset in the field. A
  linker reads the two the same.
- Nothing is preempted in a COFF object, so a reference to a symbol in its own
  section is resolved whatever its binding, except a call to a function
  (`.type 32`), which llvm-mc leaves to the linker for incremental linking and
  control flow guard; GNU as resolves that and relocates a call to a weak
  definition instead.
- A weak definition hides behind `.weak.<name>.default.<first global>`, where
  GNU as leaves out `.default`.
- A sign-extended 32-bit field is `IMAGE_REL_AMD64_ADDR32`, the only 32-bit
  absolute type the PE specification has; GNU as writes type 17.
- Code is padded with llvm-mc's no-ops: up to fifteen bytes at once for
  x86-64, one-byte `nop`s for i386, whose default Windows CPU has no `nopl`.
- On i386, a local label spelled with a leading `L` is private, as in
  llvm-mc's Microsoft conventions; elsewhere `.L` is.

The debugging sections are the exception: they follow whichever assembler
checks the target's DWARF in any format, which for x86 is GNU as, so an x86
PE object's `.debug_*` and `.eh_frame` are GNU as's and differ from llvm-mc's
in version, file table and CIE alike. Everything else in the object still
follows llvm-mc, and on ARM64, which mingw has no assembler for, so do the
debugging sections.

Neither reference writes `IMAGE_REL_AMD64_REL32_1` to `_5`: both measure every
PC-relative field from four bytes past it and put the difference in the field,
so rsasm does the same. Four things differ on purpose: ELF's `.type
foo,@function` is accepted and says nothing, where llvm-mc refuses it; and
three are refused where llvm-mc 22 cannot answer either, but answers with a
crash or an unhelpful diagnostic — a `.comm` alignment past 32 bytes, a
`.cv_filechecksums` whose file table has a number skipped, and a CodeView
field naming a file's place in a checksum table the source never asked for.

In NASM source the object follows NASM's COFF writer rather than llvm-mc's:
its section words (`code`, `data`, `rdata`, `bss`, `info`, `align=`) and
characteristics, only the sections the source named or filled, no
checksums, its `.file`, `.absolut` and (for i386) `@feat.00` symbols, and
relocations against a defined symbol's section.

## Mach-O objects

`-f macho`, or a target triple for Darwin in `-a`, writes an `MH_OBJECT` for
x86-64, i386 or arm64:

```console
$ rsasm -a arm64-apple-macos -o hello.o hello.s
```

The source is Darwin's assembly, as llvm-mc reads it for those triples:

- **Sections are segment and section pairs.** `.section __DATA,__data`, with
  an optional type, `+`-joined attributes and stub size
  (`.section __TEXT,__cstring,cstring_literals`), and the shorthands `.text`,
  `.data`, `.bss`, `.const`, `.const_data`, `.cstring`, `.literal4`,
  `.literal8`, `.literal16`, `.mod_init_func`, `.mod_term_func`,
  `.non_lazy_symbol_pointer`, `.lazy_symbol_pointer`, `.tdata`, `.tlv` and
  the rest of llvm-mc's list. `.zerofill`, `.lcomm` and `.comm` reserve
  zero-filled space, and their alignment, like `.align`'s, is a power of two.
  There is no `.rodata`.
- **Symbols.** A label whose name starts with `L` is the assembler's own;
  every other name, `l_.str` included, reaches the symbol table, with no
  underscore added. `.globl`, `.private_extern`, `.weak_definition`,
  `.weak_reference`, `.alt_entry`, `.no_dead_strip` and
  `.subsections_via_symbols` say what the linker may do with them.
- **Relocation modifiers** are Darwin's: `sym@GOTPCREL` on x86-64, and
  `sym@PAGE`, `sym@PAGEOFF`, `sym@GOTPAGE`, `sym@GOTPAGEOFF` and `sym@GOT` on
  arm64, where `:lo12:` is not accepted.
- **A thread-local variable** is reached through a descriptor the loader fills
  in for each thread, not through a place in a thread's block as in ELF:
  `sym@TLVP` in a RIP-relative operand on x86-64, and `sym@TLVPPAGE` with
  `sym@TLVPPAGEOFF` on arm64, neither of which takes an addend. The
  descriptor itself is three words the source writes in
  `__DATA,__thread_vars` (`.tlv`), and the initial value goes in
  `__DATA,__thread_data` (`.tdata`) or, where it is zero,
  `__DATA,__thread_bss`, which `.tbss symbol,size[,align]` reserves.
- **The deployment target** is the release of the system the object is for,
  and a macOS version in the target triple names it:
  `-a x86_64-apple-macos10.6` writes `LC_VERSION_MIN_MACOSX`, and from macOS
  10.14 on the newer `LC_BUILD_VERSION`, which names its platform as well. A
  Darwin kernel version (`x86_64-apple-darwin19`) names the release that
  shipped with it, and on arm64, which no macOS before 11.0 ran, an older
  version is raised to that. The triple is also what decides whether the
  object has a compact unwind table at all; see
  [Compact unwind](#compact-unwind).
- `.build_version` writes `LC_BUILD_VERSION` and `.macosx_version_min`,
  `.ios_version_min`, `.tvos_version_min` and `.watchos_version_min` write the
  older `LC_VERSION_MIN_*`; the last of them wins, and replaces the triple's,
  since llvm-mc keeps one deployment target and writes it either way. What a
  directive cannot change is the compact unwind table, which llvm-mc settles
  before it reads a line of source. `.data_region` with
  `.end_data_region` writes `LC_DATA_IN_CODE`, `.linker_option "-lfoo"` an
  `LC_LINKER_OPTION` of arguments for the linker, and `.indirect_symbol` fills
  the indirect symbol table `LC_DYSYMTAB` points at -- one entry per pointer
  or stub, naming the symbol it stands for, or saying that the linker needs no
  symbol for a pointer this file defines itself. On arm64 `;` starts a
  comment.
- **Debugging information** goes in a `__DWARF` segment of its own and in
  `__TEXT,__eh_frame`, as llvm-mc writes it; see
  [Debug information](#debug-information). On arm64 a frame is described in
  `__LD,__compact_unwind` as well, and often only there, and on x86-64 from
  macOS 10.6 on; see [Compact unwind](#compact-unwind).

**A 32-bit object is narrower, and relocates differently.** i386 writes an
`mh_magic` header with no reserved word, an `LC_SEGMENT` of `section` records
rather than `section_64` ones, `nlist` entries rather than `nlist_64`, and
pads the file to four bytes rather than eight. Its relocations are Mach-O's
generic ones, which are the same on every 32-bit machine: one
`GENERIC_RELOC_VANILLA` for everything a field can hold, naming the target's
section wherever the field can be worked out here and the symbol only where
it cannot -- an undefined symbol, or a weak definition the linker may replace.
A PC-relative field keeps the whole distance from the end of the field, which
for a record naming a symbol is the distance as if the symbol were at zero,
since the linker adds its address to what it finds; x86-64's own relocations
are measured by the linker and carry the addend alone.

Two references need more than that, and get a **scattered** record, which
carries an address where the ordinary one carries a symbol index:

- A difference of two symbols. There is one relocation type and no addend
  field, so the only way to say `A - B` is to record both addresses, in a
  `GENERIC_RELOC_SECTDIFF` and the `GENERIC_RELOC_PAIR` written behind it
  holding B's. `LOCAL_SECTDIFF` is the same relocation where the symbol added
  is not a global one; the linker reads the two alike, and llvm-mc tells them
  apart as Darwin's own assembler did.
- A reference some way into a symbol whose section the record would otherwise
  name, since a section cannot say which of the symbols in it the field was
  measured from. The record holds the address it was measured from and the
  linker moves the field by however far that address moves. A reference with
  no offset needs none, and nor does one in a debugging section, where
  llvm-mc names the section whatever is in it.

`sym@TLVP` is i386's too, written in an absolute operand rather than a
RIP-relative one, since the machine has no PC-relative addressing, and in the
`sym@TLVP-L0$pb(%eax)` form position-independent code reaches a descriptor
through: one `GENERIC_RELOC_TLV` naming the descriptor, with the distance from
the PIC base left in the field.

A `.cfi_*` register is in Darwin's own i386 numbering, which swaps `%esp` and
`%ebp` against the psABI's (LLVM's `DWARFFlavour::X86_32_DarwinEH`);
`.debug_frame` takes them back, as llvm-mc does.

**What is left to the linker is decided by atoms, not by binding.** A Mach-O
linker may move or drop the code from one linker-visible label to the next on
its own, so a reference from one of these atoms into another is relocated
however close the two are, even to a local symbol, and against the target's
atom with the distance as the addend; one within an atom is resolved. On arm64
llvm-mc resolves a branch to any label in the same section unless the file
has `.subsections_via_symbols`, which is what promises the linker real atoms,
and rsasm does the same. A difference of two labels is a `SUBTRACTOR` pair
unless both are in one atom — or, as llvm-mc folds it while reading a data
directive, a fixed distance apart there, already defined. Where Mach-O has no
relocation for something ELF can express — `adr` or a conditional branch to
another atom, a 32-bit absolute address on x86-64, a page reference without
`@PAGE` — the reference is refused, as llvm-mc refuses it.

### Compact unwind

Darwin's linker does not read the frame table first. For each function it
reads one 32-bit word out of `__LD,__compact_unwind` and goes to
`__TEXT,__eh_frame` only where that word says the frame is a shape no word
can describe, so a frame described in one of the two alone is described
wrongly. On arm64, where llvm-mc writes the table for every triple, rsasm
writes it too: an entry per `.cfi_startproc` holding the function, its
length, the word, and the personality routine and language-specific data area
that frame's `.cfi_personality` and `.cfi_lsda` named. The entry is the
thirty-two bytes the linker reads, unless a `.cfi_lsda` encoding narrower
than a pointer leaves it short, which is what llvm-mc writes there too.

The word is `DarwinAArch64AsmBackend::generateCompactUnwindEncoding`'s, which
reads the directives in the order a compiler writes them:

- `UNWIND_ARM64_MODE_FRAME` for a frame pointer prologue: a `.cfi_def_cfa`
  naming `w29`, followed immediately by the `.cfi_offset`s that saved `w30`
  and `w29`, eight bytes apart. Each further pair of callee-saved registers
  (`x19`/`x20` up to `x27`/`x28`, then `d8`/`d9` up to `d14`/`d15`) sets a
  bit of its own, and has to be saved in register order and eight bytes below
  the pair before it.
- `UNWIND_ARM64_MODE_FRAMELESS` with the stack adjustment of the frame's one
  `.cfi_def_cfa_offset`, counted in sixteen-byte units and up to 65,520
  bytes. A frame with no directives at all is this with no adjustment, and
  that answer comes before the personality routine is looked at, so even a
  routine with no slot of its own leaves it alone.
- `UNWIND_ARM64_MODE_DWARF` for everything else — a second stack adjustment,
  a `.cfi_def_cfa_register`, registers saved out of order or alone, an
  escape, a state change, a personality routine other than
  `___gxx_personality_v0` or `___objc_personality_v0` — and that is the one
  word that also puts the frame in `__TEXT,__eh_frame`, which is written for
  those frames and no others.

On x86-64 the table arrives with the deployment target: llvm-mc writes one
only where the triple names a macOS of 10.6 or later (`useCompactUnwind`),
the release whose linker first read one, so `-a x86_64-apple-macos` has none
and `-a x86_64-apple-macos10.6` has. Unlike arm64's, x86-64's word never
describes a frame on its own (`getSupportsCompactUnwindWithoutEHFrame`), so
every frame stays in `__TEXT,__eh_frame` as well.

The word is `DarwinX86AsmBackend::generateCompactUnwindEncoding`'s, and where
arm64 gives each pair of saved registers a bit, x86-64 writes the registers
themselves — `%rbx`, `%r12` to `%r15` and `%rbp`, numbered 1 to 6:

- `UNWIND_X86_64_MODE_RBP_FRAME` for a frame pointer prologue, which is a
  `.cfi_def_cfa_register` naming `%rbp`. Everything before it set the frame
  pointer up and the mode says so by itself; everything after is the
  registers saved below it, three bits each in the order they were saved,
  five of which is all the word has room for and the sixth of which llvm-mc
  drops rather than give up. The lowest of them has to sit twenty-four bytes
  below the CFA, directly under the saved frame pointer, but they may be
  saved in any order and the ones after the first anywhere.
- `UNWIND_X86_64_MODE_STACK_IMMD` with the stack adjustment of the frame's
  last `.cfi_def_cfa_offset`, in eight-byte units and up to 2,040 bytes,
  followed by the number of registers saved and which permutation of the six
  they are — the number of an arrangement rather than the registers, which is
  what makes six of them fit in ten bits.
- `UNWIND_X86_64_MODE_STACK_IND` where the adjustment is larger than that:
  the linker reads it out of the `subq` that made the frame, and the word
  carries the offset of that instruction's immediate instead, counted past
  the pushes that saved the registers.
- `UNWIND_X86_64_MODE_DWARF` for everything else — a CFA register that is not
  `%rbp`, a seventh saved register or one the word cannot name, a register
  saved at the wrong distance below a frame pointer, a `.cfi_def_cfa`, an
  escape, a state change, a personality routine other than
  `___gxx_personality_v0` or `___objc_personality_v0`.

A frame with no directives at all gets no word and no entry, where arm64
gives it a frameless one; a table that would hold no entries is not written.

`tools/macho-diff/run.sh` compares 3,085 cases against llvm-mc 22: single
statements and whole programs in Clang's style of its own, line tables, frame
tables and compact unwind tables, and the `tools/mc-diff` corpora for all
three machines, every instruction of which has to come out the same in a
Mach-O object. Every header and load command, section, symbol and relocation
matches, and each of the 3,001 objects both assemblers write is identical
byte for byte; the other 84 cases are refused by both.

Five differences remain, and the corpora leave them out:

- x86-64 instructions are encoded as GNU as encodes them, in either format, so
  alignment padding in code uses GNU as's no-ops, and `addl $sym, %eax` is the
  short `05` form where llvm-mc writes `81 c0`.
- A negative addend on an arm64 branch or page reference is written as a
  24-bit two's-complement `ARM64_RELOC_ADDEND`. llvm-mc 22 writes the addend
  over the entry's type and length bits, which not even llvm-readobj can read
  back.
- A reference through the GOT on arm64 to a label some way into its atom is
  refused, since a GOT relocation has no addend; llvm-mc accepts it,
  relocating against the atom and writing the offset into the instruction.
- An ELF thread-local operator in a 32-bit object -- `foo@TPOFF`,
  `foo@GOTTPOFF` and the rest -- is refused, as it is in a 64-bit one.
  llvm-mc's 32-bit Mach-O writer looks at a modifier only to see whether it
  is `@TLVP` and otherwise drops it, writing the plain relocation and so
  assembling a different program from the one the source asked for; its
  64-bit writer refuses the same operators outright.
  `tools/macho-diff/i386-skip.txt` names the lines of the shared corpus this
  leaves out.
- `.cfi_personality` in a 32-bit object is refused. Darwin's CIE reaches the
  routine through the GOT, which a 32-bit object has no relocation for, and
  llvm-mc writes the distance to the routine itself -- as a difference of the
  routine and a label at the field, which it then refuses for any routine the
  object does not define, so a compiler's `___gxx_personality_v0` is refused
  either way. For a routine the object does define, llvm-mc writes a
  `SECTDIFF` naming that label; rsasm has no label inside the frame table to
  name and refuses the frame with the reason.

## Linking

An object file is not a program: its addresses are not final, and nothing in
it has resolved a reference from one file to another. `--link` finishes the
job in the same process, through [qld](https://github.com/KarpelesLab/qld), a
linker written in Rust, so that what `-o` names is something that runs:

```console
$ rsasm --link -o hello examples/hello.s
$ ./hello
Hello from rsasm!
```

Nothing is written between the two steps. The object exists only as bytes in
memory, and the image comes back the same way, which is why `--link` needs
neither a temporary file nor a second process. Whatever the link has to
report arrives through rsasm's own diagnostics, in the shape a refused
instruction has, and a link that fails exits non-zero instead of leaving
something unrunnable behind:

```console
$ rsasm --link -o bad bad.s
error: bad.o:(.text+0x1): undefined symbol: nowhere
rsasm: link failed: 1 error
```

A program that calls into a library needs the library and the loader that
will find it, which `-l`, `-L` and `--dynamic-linker` name exactly as they do
for GNU ld. `-l:<file>` names a file in the search path rather than a
`lib<name>`, which is how a C runtime's startup objects are reached:

```console
$ rsasm --link -o hi -L/usr/lib64 --dynamic-linker /lib64/ld-linux-x86-64.so.2 \
    -l:crt1.o -l:crti.o -lc -l:crtn.o hi.s
```

The libraries are searched after the assembled object, in the order they were
given. `-e` names the entry symbol, which is `_start` otherwise, and `-shared`
and `-pie` ask for a shared library or a position-independent executable in
place of the fixed-address static executable `--link` produces on its own.

qld links ELF and PE32+ images for x86-64 and AArch64, which is a small part
of what rsasm assembles, so a link it cannot do is refused before anything is
assembled rather than after:

```console
$ rsasm --link -a riscv64 -o never prog.s
rsasm: `--link` cannot make a program out of elf output for riscv64; qld links ELF and PE32+ objects for x86-64 and AArch64
```

Linking is the `link` cargo feature, and qld is the only dependency rsasm has.
The feature is in the default set, because an assembler that stops at an
object leaves the last step to some other tool; `--no-default-features` gives
a crate with no dependencies at all, and a build made that way says so plainly
when it is asked to link.

## Verification

Ten differential harnesses assemble the same source with rsasm and with an
independent assembler, and compare the bytes.

Where a harness compares whole objects, it compares every section a
reference writes of its own accord along with the ones the source asked for:
the build attributes (`.ARM.attributes`, `.riscv.attributes`,
`.MSP430.attributes`, `.gnu.attributes`), MIPS's `.reginfo`,
`.MIPS.options` and `.MIPS.abiflags`, V850's `.note.renesas`, AVR's
`.avr.prop` — and the header's `e_flags`. Those sections say what a linker
and a loader may do with the object, and leaving them out of the comparison
is what hid them from rsasm for as long as it did.

- `tools/gas-diff/run.sh` against GNU as 2.47, for x86 in 64-, 32- and
  16-bit mode, in AT&T and Intel syntax. 12,552 of 12,552 match.
- `tools/mc-diff/run.sh` against llvm-mc 22, for x86 and the targets LLVM
  supports. 45,970 of 45,970 match across twenty-three target variants. For RISC-V
  it also compares whole objects, relocations included, since `la` and its
  relatives are only right if the linker is told the right things; llvm-mc runs
  with `+relax` there, because relaxation is on in rsasm as it is in GNU as,
  and the `R_RISCV_RELAX` marks are left out of the comparison because llvm-mc
  puts them beside relocations GNU as does not (the corpus heads say which).
- `tools/xas-diff/run.sh` against cross GNU as 2.47 for m68k (for each CPU
  model, with corpora generated from GNU's own opcode table so that every
  form in it is assembled), SuperH, RX, RL78,
  V850/RH850, AVR, MSP430 and the Z80, vasm for Motorola syntax and for the Z80 and the
  6502, cc65's ca65 for the 6502, AS for the 8080 and the 78K0, and AS and
  SDCC's sdas8051
  for the 8051 (its Intel HEX against AS's `p2hex`), plus CC-RL, CC-RH and
  CC-RX source paired with its GNU-syntax equivalent. For ARM and Thumb it
  compares whole objects, local and mapping symbols included, against GNU as,
  the reference for literal pools and interworking, and for AArch64's literal
  pools and system instructions; for PowerPC's vector and
  POWER8–10 instructions it is GNU as's second opinion, and the check on the
  forms only GNU as accepts. `tools/oracles/build.sh` builds the references
  from checksum-pinned sources. 25,719 of 25,719 match across fifty-seven
  variants.
- `tools/flat-diff/run.sh` against a link, for flat binaries: the reference
  assembler's object, linked by GNU ld 2.47 at the same base address with the
  sections laid end to end, against `rsasm -f bin`. That is what checks the
  arithmetic a linker would otherwise do — `adrp` pages, `@ha`, `%pcrel_lo`,
  distances between sections. 205 of 205 match across thirty-one variants.
  `tools/oracles/build.sh` builds the linkers alongside the assemblers.
- `tools/link-diff/run.sh` against a link of a *whole program*: two or three
  objects that reference each other, assembled by the reference assembler and
  by rsasm, each set linked by the same GNU ld 2.47 with the same script, and
  the linked images and symbol tables compared. Bytes alone cannot show a
  relocation that names the wrong symbol or carries the wrong addend — the
  field it covers is zero in both objects — and flat-diff only ever links one
  object, so nothing else here depends on a symbol being resolved across a
  file boundary. Per target the programs cover calls and branches between
  objects, absolute and PC-relative data references with addends, the halves
  of an address (`@ha`/`@l`, `%hi`/`%lo`, `:lo12:`, `:abs_g1_nc:`,
  `:lower16:`, `hi()`/`lo()`), literal pools and constant pools loading
  another object's symbols, ARM/Thumb interworking, the GOT and PLT operands
  where the backend has them (`@GOTPCREL`, `@GOT`, `@PLT`, m68k's `@GOTPC`
  and `@PLTPC`, ARM's `sym(GOT)`),
  the x86, AArch64, PowerPC, RISC-V and SPARC thread-local access models,
  which the linker turns into local exec, ARM's and Thumb's, whose descriptor
  calls it turns into initial exec, all five MIPS models and its
  position-independent operators (`%got`, `%call16`, `%gp_rel` and the
  `%got_hi`/`%got_lo` pair, which measure from the `_gp` the script defines
  for them), weak definitions a second object overrides, `.comm`
  symbols merged between objects with different sizes, `.bss`, and
  references into another object's sections. The targets whose linker
  relaxes — SuperH, RX, RL78, MSP430, V850/RH850, AVR and RISC-V — are linked
  a second time with `--relax`, which is what their difference records,
  `R_MSP430_SYM_DIFF` pairs, `.avr.prop` and RISC-V's `R_RISCV_RELAX` and
  `R_RISCV_ALIGN` exist for: the relaxed image has to come out the same from
  both assemblers' objects, alignment and all. Two more rows link
  [PE/COFF](#pecoff) objects into an image with GNU ld for mingw, where what
  a link has to get right is `@IMGREL`, `.secrel32` and `.secidx` and the
  addend a COFF relocation keeps in its field. 298 of 298 match across
  twenty-nine variants.
- `tools/nasm-diff/run.sh` against NASM 2.16.03, for the `nasm` dialect: whole
  programs compared as flat binaries, as ELF objects, relocations and global
  symbols included, and as `win64` and `win32` COFF objects. 444 of 444
  match. `tools/oracles/build.sh` builds NASM from a checksum-pinned source.
- `tools/multiarch-diff/run.sh` for files that switch targets with `.arch`,
  against the same references, one part at a time.
- `tools/dwarf-diff/run.sh` for [debug information](#debug-information),
  against GNU as 2.47 or llvm-mc 22, whichever the target follows: the line
  table, frame and compilation unit sections byte for byte with their
  relocations, from hand-written snippets, `-g` and whole files from GCC and
  Clang. Three of the variants are the same targets as PE objects, against
  the mingw assembler for x86 and llvm-mc for ARM64, compared as the
  debugging sections' characteristics and bytes and every relocation with
  the addend its field holds; the Mach-O side is in `tools/macho-diff`,
  where llvm-mc is the reference for the whole object. 1,425 of 1,425 match
  across twenty-nine target variants.
- `tools/coff-diff/run.sh` for [PE/COFF objects](#pecoff), against llvm-mc 22
  for x86-64, i386 and ARM64 as whole objects — every section's
  characteristics and bytes, every symbol with its auxiliary records, every
  relocation — from single statements, hand-written programs and Clang's
  output, and against GNU as 2.47 for mingw as relocations with the addends
  their fields hold. 348 of 348 comparisons match. `tools/oracles/build.sh`
  builds GNU as for mingw alongside the other cross assemblers.
- `tools/macho-diff/run.sh` for [Mach-O objects](#mach-o-objects), against
  llvm-mc 22 for x86-64, i386 and arm64: header, load commands, sections,
  symbols and relocations as `llvm-readobj` reads them, over its own corpora
  and those of `tools/mc-diff`. 3,085 of 3,085 match, and every object both
  assemblers write is also identical byte for byte.

The x86 backend is also fuzzed: `tools/fuzz/x86.py` generates random
instructions, in all three modes and both syntaxes, some of them deliberately
invalid, and compares rsasm's bytes, relocations and accept/reject decision
with GNU as's and llvm-mc's. The general-purpose forms are written from the
Intel manual; the SIMD and newer extensions are read from GNU's expanded
opcode table, every row of them, with writemasks, broadcasts, rounding and
displacements at every disp8\*N scale. The x86 SIMD tables were derived from
that same table (`tools/fuzz/gnutbl.py` decodes it). Where the two references
disagree, rsasm follows GNU as, apart from the few cases the corpora note;
runs of 600,000 general-purpose and 240,000 mixed instructions find no case
where rsasm differs from both. The m68k backend is fuzzed the same way:
`tools/fuzz/m68k.py` draws instructions from GNU's own opcode table, read out
of the binutils source, for each CPU model in GNU and Motorola syntax against
GNU as, and in Motorola syntax against vasm; runs of 400,000 and 100,000
instructions find nothing. The table rsasm encodes those forms from,
`src/arch/m68k/table.rs`, is written from the same source by
`tools/tables/m68k.py`. `tools/fuzz/msp430.py` does the same against GNU as
alone for the MSP430's 430, 430X and 430Xv2 instruction sets, and 200,000
cases find no difference but the deviations the backend documents. The 8051
is fuzzed with whole programs:
`tools/fuzz/mcs51.py` assembles them with AS and sdas8051 too, and 80,000
programs find no case where rsasm differs from the references outside the
places this README describes. AVR is too: `tools/fuzz/avr.py` generates
programs from every row of GNU as's opcode table — labels in several
sections, branches near and out of reach, modifiers, data and alignment — on
twenty-one cores, compares whole objects with `avr-elf-as`'s and, for a
program with nothing undefined, the image `avr-elf-ld` links from it with
`rsasm -f bin`; 120,000 programs, 37,000 of them linked, find no case where
rsasm differs outside the deviations this README lists.

Every other backend is fuzzed too, and all of them from one place. RISC-V
draws its forms from the operand format strings in binutils' `riscv-opc.c`
and goes to llvm-mc and `riscv64-elf-as`; MIPS, SPARC, SuperH, RX, RL78,
V850/RH850 and the Z80 take their cases from the *other* side of binutils —
GNU objdump's disassembly of random bytes, which reaches every operand value
a form allows and shares nothing with either assembler's parser — and go to
whichever references that target has. The 6502, the 8080 and the 78K0 are
fuzzed with whole programs against ca65 and the Macro Assembler AS, as the
8051 already was, and `nasm` source against NASM itself.
`tools/fuzz/run.sh` runs all twenty-one with one seed and bounded counts —
768,000 cases in a little over two
minutes on a four-core runner — which is what CI runs on every pull
request; the nightly run uses the date as its seed and ten times the cases.
A fuzzer that compared nothing fails the job as loudly as one that found a
difference. See `tools/fuzz/README.md`.

AArch64's SIMD, floating-point and SVE table is derived from llvm-mc rather
than written: `tools/tables/aarch64.py` disassembles random instruction words
to find every form llvm-mc prints, measures where each operand's bits go by
assembling the form with one operand changed at a time, and checks every form
against llvm-mc before writing `src/arch/aarch64/table_data.rs` (6,794 forms) and
the corpora that check it, `tools/mc-diff/aarch64-{simd,sve,gp}-words.txt`
(20,329 lines, compared a batch at a time). The general-purpose groups that
are families of the same shape go through it too, and are the `gp` corpus:
the load/store exclusives and the acquire/release accesses, the LSE, LSE128,
LSUI, RCPC and FEAT_THE atomics, FEAT_LSCP's acquire-release pair, the
pointer-authentication instructions that
name a register (the `paciasp`-style hints, which both references treat as
mnemonics of their own, stay in `insn.rs`), memory tagging, the CRC32
checksums, and the unprivileged `ldtr`/`sttr` and byte and halfword
`ldapurb`/`stlurb` unscaled accesses — one form per access size and
signedness over a 9-bit signed offset, which is the shape a row holds.
FEAT_LSFE's floating-point atomics come in with the `simd` corpus instead,
since `ldfadd s0, s1, [x2]` names floating-point registers.
A general-purpose instruction that is *one* form rather than a family goes to
`insn.rs` instead, because each of those constrains an operand in a way a
measured row has no way to say: `bfc` computes its rotation from its field
width and bounds their sum, `ld64b` and `st64b` name the first of eight
consecutive registers so only an even one up to `x22` will do, `rprfm` reads
an operation name or a six-bit number scattered over the word, `udf` has no
opcode bits at all, and `rmif`, `setf8`, `cfinv`, `ctz`, `wfet`, `maddpt` and
`bc.<cond>` each join a family `insn.rs` already writes out. Two families are
there for the same reason: FEAT_MOPS's 132 memory copies and sets, whose three
registers must all be different and whose operands are an address and a
register written back with nothing for the `!` to apply to, and FEAT_CMPBR's
thirty compare-and-branches, whose target is a fixup no row can hold.
`tools/tables/aarch64.py check` says whether the table is still what llvm-mc
gives. The backend is fuzzed by `tools/fuzz/aarch64.py`, whose cases are
llvm-mc's or GNU objdump's disassembly of random words, a quarter of them
mutated into likely-invalid ones; runs of 500,000 instructions find no case
where rsasm differs from both references. Where the two disagree, rsasm
follows llvm-mc for what an instruction means and GNU as for what is out of
range: llvm-mc takes
`ext v0.8b, v1.8b, v2.8b, #8` or `scvtf s0, w0, #33` and truncates them,
where GNU as and rsasm refuse them. It also refuses the SVE spellings only
llvm-mc reads — an unpredicated `and z0.s, z0.s, z1.s`, whose element size is
always `.d`, and an immediate outside the element's signed range, such as
`mov z0.h, #-65408` — and takes the ones only GNU as reads: `fcmp s0, 0` for
`#0.0`, and a register list `{z0.h - z1.s}` of two element sizes is refused
as llvm-mc refuses it. Two of the new groups' forms go the other way and
are unpredictable rather than unencodable, so GNU as warns and assembles
them and rsasm does too, where llvm-mc refuses them: a store-exclusive whose
status register is also one of its sources (`stxr w0, x0, [x1]`), and an
`ldraa` that writes back the register it loads into. A third goes the same
way for a plainer reason: GNU as reads an explicit zero offset in a 64-byte
access (`ld64b x0, [x1, #0]`) where llvm-mc takes the base register alone.
`smstart`, `smstop` and `zero {za}` are handwritten.

The system instructions are generated the same way from the other reference:
`tools/tables/aarch64-sys.py` takes the names from binutils' own tables —
`opcodes/aarch64-sys-regs.def` and the `aarch64_sys_regs_*` arrays — and
every encoding from a run of `aarch64-elf-as`, writing
`src/arch/aarch64/sysreg_data.rs` (1,619 `mrs`/`msr` registers with what each
allows, 12 PSTATE fields, 284 `dc`/`ic`/`at`/`tlbi` operand names and 73
aliases of `hint`) and a line per name to whichever corpus can check it:
`tools/mc-diff/aarch64-sys-words.txt` where llvm-mc gives the same word, and
`tools/xas-diff/aarch64.txt` where it does not know the name at all, which is
most of the newer ones. `msr` of a register the architecture says is
read-only warns, as GNU as warns; neither assembler refuses it.

Literal pools are GNU as's feature, so GNU as is the reference for them:
`tools/xas-diff/aarch64-relocs.txt` compares whole objects, mapping symbols
and relocations included, and `tools/flat-diff/aarch64-gas.txt` compares
linked images. Unlike its own ARM port, and unlike llvm-mc — which turns
`ldr x0, =1` into `mov x0, #1` and writes its entries in the order they were
used — GNU as on AArch64 always loads from the pool, groups the entries by
width, aligns each run and shares an entry between loads of the same value:
rsasm does what GNU as does.

PowerPC's AltiVec, VSX and POWER8–10 instructions are fuzzed the same way by
`tools/fuzz/powerpc.py`, which draws its forms from the operand kinds in GNU
binutils' opcode table and runs all three PowerPC targets. A run of 600,000
instructions finds no case where rsasm differs from both references other
than the two refusals it makes on purpose: a doubleword instruction in 32-bit
code, and a register name from another bank (`%vs3` where a general-purpose
register goes), which both read as its number. The instruction table behind
them, `src/arch/powerpc/vector.rs`, is written from that same opcode table by
`tools/tables/powerpc.py`, never by hand.

ARM and Thumb are derived and fuzzed the same way. `tools/tables/arm.py`
reads the five tables in binutils' `opcodes/arm-dis.c` — the A32, 16-bit
Thumb, 32-bit Thumb, coprocessor and NEON ones — and turns each row's format
string, which spells out where the disassembler finds every operand, into
the form an assembler encodes from, writing `src/arch/arm/table.rs` (2,523
forms under 712 spellings). Every row is accounted for: it becomes a form,
its mnemonic belongs to a hand-written encoder (the ones whose bytes depend
on more than the operands — the data-processing group with its Thumb width
selection, the branches, the literal pool, `ldm`/`stm`, `it`, `cbz`,
`msr`/`mrs`), it is a spelling only the disassembler prints, or its
architecture is out of scope; a row that is none of those is an error, and
`tools/tables/arm.py check` says whether the file is still what binutils
gives. Where the disassembler's table is looser than the instruction set —
it will print `vneg.f8`, a `vext` immediate too wide for its registers, or a
quadword register where only a double one goes — the restriction comes from
the operand kinds of gas's own `insns[]`, and what neither table says is
written out in the script with the reason. `tools/fuzz/arm.py` then
generates random instructions from those same format strings, read again and
independently, and compares rsasm against GNU as and llvm-mc in both
instruction sets; runs of 20,000 instructions find no case where rsasm
differs from both. GNU as takes a condition on `vaddl` and `vsubl`, alone of
the NEON instructions, which is the one recorded deviation.

The first three also compare whole objects for every ELF target, from the
`*-relocs.txt` corpora: each allocated section's type, flags, size, alignment
and bytes, the global, weak and undefined symbols, and every relocation, read
the way a linker reads it (`tools/mc-diff/canon.sh`). Bytes alone cannot show
a reference that should have been left to the linker, or one relocated
against the wrong symbol: the field is zero either way. Those corpora walk
each binding — local, global, weak, hidden and the other visibilities, `.set`
aliases either way round, `.globl` after use, another section, undefined —
through branches, calls, PC-relative loads and data.

All ten run in CI. The expected bytes in the hermetic tests under `tests/` were
taken from these runs rather than written by hand: a test that only checks
rsasm against rsasm can never find a wrong encoding.

Where rsasm and the reference legitimately differ, the corpus says so rather
than dropping the case. The standing example is alignment padding in
executable sections, where GNU as picks its no-op sequence by `-mtune`; only
the total length is fixed. In flat binaries it is branches between sections:
a reference assembler cannot know how far away another section will be and
takes the longest form, while rsasm, which lays the image out itself, takes
the shortest that reaches; the flat corpora write those widths out.

ARM has two references that disagree with each other. llvm-mc checks the
encodings; GNU as, which the source was written for, decides everything that
depends on more than one instruction: where literal pools go and what they
share, mapping symbols, which branches become `blx` and which are left to
the linker, how relaxation sizes Thumb instructions, and that a code
section's end is padded to a word. rsasm follows
GNU as there, and llvm-mc where the two only differ in spelling: Thumb
alignment padding uses 16-bit no-ops, and `adds r0, r0, #1` keeps the
three-operand form, where GNU as uses 32-bit no-ops and the 8-bit form. In
the vector instructions it is llvm-mc that is the looser of the two, and
rsasm follows GNU as: a condition, a width suffix or an immediate wider than
the element size is refused on a NEON instruction, as is a quadword register
where only a double one goes. The exception is the `al` condition, which
llvm-mc takes everywhere and GNU as refuses on anything unconditional --
except where some other form of the mnemonic is conditional, which is how it
comes to take `vnegal.f32 d0, d1`; rsasm takes it everywhere. Two things GNU
as refuses are assembled here, since llvm-mc writes for both the object rsasm
writes: an ARM branch to a local label an odd number of halfwords into Thumb
code in another section, whose offset GNU as checks before the linker has
placed the section, and a literal pool entry holding a difference of labels
its parser cannot fold (`ldr r0, =l1-l0`). See `tools/xas-diff/README.md` and
the ARM backend's module documentation.

## Design

The interesting problems in an assembler are mostly about *when* things are
known, and the design is shaped around that.

**Lexing depends on the target, not just the dialect.** `;` separates
statements in GAS and starts a comment in NASM; `1b` is a local-label reference
in GAS and the binary constant `2` in NASM. Within GAS, `#` is a comment on x86
but the immediate prefix on ARM, AArch64 and SPARC, where it is a comment only
in the first column — which is also what C preprocessor line markers look like.
So each backend supplies its comment syntax, and the lexer is configured from
it. A file is read one statement at a time, each after the one before it has
been assembled, which is what lets an `.arch` switch change how the next line
is spelled without a second pass or a guess.

**The parser stops at the statement level.** It finds labels, directives and
mnemonics; it does not look inside operands. `disp(base,index,scale)` and
`[base + index*scale + disp]` are x86's problem, and register lists are ARM's,
so the architecture backend gets the raw token tail and parses it itself.

**Expressions evaluate to relocatable values,** not numbers: `plus - minus +
addend`. That is what lets `end - start` be a constant while `end` alone is
not, and it is the same representation whether the answer comes out as bytes
or as a relocation.

**Positional references are bound where they are written.** `.` and `1f` mean
different things depending on where in the file they appear, which is
information that no longer exists by the time an expression is evaluated. Both
are rewritten into ordinary symbol references as soon as the statement
containing them is parsed.

**Macro expansion is textual, and re-lexed.** Substituting tokens cannot
express what macro bodies rely on: `.L\@_loop:` has to paste the invocation
counter into the middle of an identifier, and there is no token meaning "join
these". Expanding into text and re-lexing gives that for free, and it is what
GNU as does, so bodies written for it behave the same way. The expansion
becomes a real entry in the source map, which turns out to be a feature — a
diagnostic inside a macro points at the expanded line and names the macro it
came from.

**A relocation is described, not just numbered.** A backend picks the ELF
relocation number for each field it leaves to the linker, and also says what
the field computes — a branch, a load through the GOT, the page of an address
(`RelocClass`) — which the ELF writer has no need of. Mach-O numbers the same
meanings differently and splits some of them further, so its writer maps the
description instead of second-guessing ELF's numbers.

**A fragment that might change size carries every candidate.** An instruction
whose branch could be short or long is encoded *both* ways at parse time; the
layout pass picks an index into that list. Since the index only ever
increases, the loop terminates. This costs a little memory and buys a
relaxation pass with no re-entry into the parser.

### Adding an architecture

Implement `arch::Architecture` and register it in `arch::lookup`. The trait is
the whole seam:

```rust
fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>>;
```

The backend receives a mnemonic and the tokens after it, and returns the
candidate encodings with their fixups. It never touches sections, symbols or
addresses — those belong to the core, which is why two backends can write into
the same file. `.arch` switches between them at any point.

Fixed-width encodings rarely have a contiguous displacement field, so a fixup
can carry a function that scatters the value through the instruction word,
together with the field's real width and alignment. That is what lets a 26-bit
AArch64 branch offset be range-checked as 26 bits rather than as the 4 bytes
it lives in.

A few conventions really are per target and have trait methods with defaults:
`comments` (which characters start one), `word_bytes` (how wide `.word` is —
2 on x86 and PowerPC, 4 on the other RISC targets), `section_align` (the
alignment a section starts with) and `defers_to_linker` (which references to
a symbol in their own section are still relocated).

Add a corpus under `tools/mc-diff/` for the new target and take the hermetic
tests' expected bytes from its runs.

## Building

```console
$ cargo build --release
$ cargo test
$ tools/gas-diff/run.sh     # needs binutils
$ tools/mc-diff/run.sh      # needs llvm-mc and llvm-objcopy
$ tools/flat-diff/run.sh    # needs cross binutils with ld, and llvm-mc
$ tools/xas-diff/run.sh     # needs tools/oracles/build.sh
$ tools/nasm-diff/run.sh    # needs NASM from tools/oracles/build.sh
$ tools/multiarch-diff/run.sh  # needs all of the above
$ tools/dwarf-diff/run.sh   # needs llvm-mc and tools/oracles/build.sh
$ tools/coff-diff/run.sh    # needs llvm-mc and tools/oracles/build.sh
$ tools/macho-diff/run.sh   # needs llvm-mc, llvm-readobj and llvm-objdump
```

## License

MIT — see [LICENSE](LICENSE).
