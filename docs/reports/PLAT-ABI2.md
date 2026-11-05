---
status: record
summary: The three platform rounds as one implementation, with the CI run all platforms passed.
date: 2026-10-20
---
# The three platform rounds as one implementation

Branch `blaze-plat-abi2`. It began at 080a93e19d (the collated branch with
`blaze-windows-abi2` merged in), then took `blaze-aarch64-abi2` (bb4e16badb),
`blaze-macos-abi2` (ec9a15955f) and `blaze-collate` at 91c7292532 (7a8d881f44).

Where each number comes from is said next to it. "CI" is GitHub Actions run 39057174862 on
888e87df74, the last commit that changes code or harnesses; what follows it is this file.
"Native AArch64" is the `ubuntu-24.04-arm` runner of that run. "qemu" is `qemu-aarch64`
user mode on the x86-64 machine the work was done on, and is not a native result. Windows
and macOS ran in CI only.

## One implementation of each

### `sym` in `naked_asm!`

There is one operand in MIR, `AsmOperand::Sym(Operand)`, one lowering of it in
`mir/lower.rs`, and one table of relocations, `Named::kind(format, arch, flags)` in
`codegen/asm.rs`, with rows for ELF x86-64, ELF AArch64, COFF x86-64 and Mach-O arm64.
A row may name an ELF type where Cranelift has no kind for it (the AArch64
`LDST*_ABS_LO12_NC`), and the object writer puts such a relocation in by its type.

Gone: the Mac branch's `AsmOperand::Function` and `AsmOperand::Static`, its `SymReloc`,
`assemble_naked` and `Naked`; the AArch64 branch's operand plumbing and its own ELF table.
What stayed is the plumbing the Windows branch had, with the table widened.

The Mach-O rows had not compiled anything after the merge: the two macOS cases
(`stabilized::arm::naked_jump`, `naked_table`) were left to rustc with "inline assembly
that names a symbol", and the jobs were green all the same (826 of 828). Two things were
wrong, both in reading the assembler's object back. The operand's name was looked up with
an underscore taken off, which an assembler does not add to a name the text has. And
`object` says of every Mach-O relocation that what is added is in the field, which is so
for an address that is data and not for an instruction, where the field is the instruction
and what is added comes apart. With a254756b77 both are compiled by blaze: 828 of 828 on
macOS under both backends.

The ELF rows on both architectures are compiled by blaze and not left to rustc: the corpus
has no function left to rustc on Linux x86-64 (822 of 822), on native AArch64 (825 of 825)
or under qemu (825 of 825), and the `sym` cases are among them.

### An argument that goes to the stack whole

`PassMode::Stacked` is gone. A 128-bit integer or an aggregate aligned to sixteen bytes
that the registers left do not hold is a `PassMode::Uniform` of one 128-bit unit, as the
Mac branch had it, for AAPCS64 and Apple alike; the alignment that decides is the one the
convention goes by (the fields' for AAPCS64, the type's for Apple).

### Variadics

One model: `VariableArguments` for a call, `Variadic` for a definition, with
`Variadic::SystemV`, `Variadic::Windows` and `Variadic::Apple { named }`.
`VariadicRegisters::all_in_registers` is gone, and `va_arg` out of a homed list (Windows)
and out of a stack list (Apple) is one arm.

`blaze-collate`'s change to how a `VaList` is passed in a C ABI touched AArch64 and
Windows x64 without having run there. After the merge the variadic cases match on all
four: they are in the corpus counts below.

## `foreign::aligned` on x86-64

The suspicion was right. Cranelift places a `StructArgument` at the next multiple of
eight, whatever the type's alignment, so an aggregate aligned to sixteen was eight bytes
early whenever the stack before it was not at a multiple of sixteen. `PassMode::OnStack`
now says whether eight bytes go ahead of it (`padded`), from the same count of stack that
places the other arguments, and LLVM takes the same classification. The case has no `cfg`
any more and matches under both backends on every platform. An argument in memory aligned
to more than sixteen bytes is refused.

## A `TargetMachine` per function on AArch64

Measured on native AArch64 (CI run 39043901289, a temporary step since removed),
`backends.py` on the corpus, three runs each, 11,813 functions:

| | producing code, s (all threads) | µs per function | blaze wall, s |
|---|---:|---:|---:|
| -O0 fast emit, machine and context kept | 9.7 to 9.9 | 821 to 838 | 7.9 to 8.0 |
| -O0 fast emit, one per function | 13.9 to 14.0 | 1177 to 1185 | 8.9 |
| -O2 fast emit, kept | 14.7 to 14.9 | 1244 to 1261 | 8.7 to 8.8 |
| -O2 fast emit, one per function | 18.7 to 18.8 | 1583 to 1591 | 9.6 |

That is about 350 µs a function and 0.9 s of wall on this corpus, which is not noise. So
the branch does not make a machine per function. The context is kept per thread with its
machines, and both are replaced together after 1024 modules: the selector's stale debug
location then never outlives the context it points into, and what a context gathers is
bounded. Resetting the selector's state alone was not tried: I found no way to it through
LLVM's C API.

The "one per function" rows were made by setting the bound to 1, which replaces the
context as well as the machine, so they are an upper bound on what 91c7292532 costs.

Mach-O arm64: the crash has not shown in any macOS LLVM job of this branch, with the kept
context. Nothing was measured there, and the jobs run -O2 with fast emit only.

## Measured

LLVM is `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`.

### CI, run 39057174862

| platform | functions from blaze | layouts that differ | LLVM compiled | left to Cranelift |
|---|---|---|---|---|
| Linux x86-64 | 822 of 822, both backends | 0 of 893 | 11,787 of 11,787 | 0 |
| Linux AArch64, native | 825 of 825, both backends | 0 of 903 | 11,813 of 11,813 | 0 |
| Windows x86-64 | 830 of 830, both backends | 0 of 898 | 11,801 of 11,801 | 0 |
| macOS arm64 | 828 of 828, both backends | 0 of 914 | 11,879 of 11,879 | 0 |

The LLVM columns are `backends.py corpus runner cases --bin runner --levels 2f`, which is
now a step of every LLVM leg.

Every leg also ran programs, stress, breakers (43 of 43, 5 open), unwinds, splices,
asmedits, includes, worktrees, keys (36 of 36, 0 open), storekills, storecorrupts and
storeshared. The Linux legs add `debuginfo.py`, `storefull.py`, `buildmodel.py` (Cranelift
leg) and, on AArch64, `farsplices.py`. macOS adds the other two modes of worktrees. What a
platform does not run is said in a comment at its step: `debuginfo.py` reads DWARF out of
an ELF executable, `buildmodel.py` builds for Linux targets, `storefull.py` mounts a
tmpfs, `farsplices.py` is about the reach of an AArch64 branch.

### `debuginfo.py` and `buildmodel.py` on native AArch64

`debuginfo.py`, Cranelift: every row right. The corpus has 11,721 functions and 12,983
parameters and locals a debugger is told of, `llvm-dwarfdump --verify` finds no error, gdb
stops and steps by line, and prints 9 of 9 locals. The locals needed a fix: they are
described from the frame pointer, which was RBP on every target and is now X29 here.
The corpus that passes has the naked and `sym` functions in it, and the row that splices
keeps the debug sections.

`debuginfo.py`, LLVM: every row right, and the locals row says "not told of: 0 described
in 21 functions: LLVM's frames are not read back yet", as on x86-64.

It failed at first on both Linux architectures in CI with "NOT STOPPED": the runners have
gdb 15, which says which of a breakpoint's places it stopped at, and the harness read
gdb 12's wording only.

`buildmodel.py`: 176 of 176 as expected, 0 wrong. Four rows are not run and say so:
`cross_musl` and `rustflags_target_cpu`, each under both backends, are of an x86-64 host
(its other libc, one of its processors). Three things were wrong before it got there, all
in the harness: the host's own target was taken for a cross one and run under qemu; those
two cases were run; and rustc's `symbols.o` was dropped from the link line, which left
`__rust_panic_cleanup` undefined in the `panic = "abort"` rows.

### Local

x86-64, Cranelift, on a254756b77's code: corpus MATCH 822 of 822, 0 of 893 layouts;
programs 22 of 22; stress 99 of 99; breakers 43 of 43, 5 open; unwinds 8 of 8; splices 71
of 71; farsplices 6 of 6; asmedits 10 of 10; includes 9 of 9; worktrees 58 of 58; keys 36
of 36; debuginfo all right; buildmodel 180 of 180 (run one commit earlier).

AArch64 under qemu, Cranelift, same code: corpus MATCH 825 of 825, 0 of 903 layouts, and
the same counts for programs through keys. `debuginfo.py` does not pass under qemu: 13
rows are wrong, the ones that run the program for its backtraces or under gdb, which the
harness does without the emulator. The native run above is what stands for it.

LLVM was not rerun locally on the last commits; the CI legs are what ran it.

## Still refused

- `sym` in an `asm!` block that is not naked, a `sym` that is a generic function, and one
  that is thread local.
- A naked function whose assembly names a symbol of its own or has data in another
  section.
- Variadic definitions: on x86-64 System V, a named parameter that is on the stack or is
  not an integer or float of at most eight bytes; on Windows, one wider than eight bytes
  or passed as an aggregate, and a definition with no named parameter; on AAPCS64 and
  Apple, a named aggregate (128-bit integers and named parameters on the stack are
  taken). `va_arg` of anything that is not one scalar, which a Rust program cannot write
  anyway, and `va_arg` on a target with none of the three conventions.
- An argument in memory aligned to more than sixteen bytes, and a vector in a C ABI.
- LLVM does not describe locals to a debugger.
- Debug info is not written for Windows, and is not checked on macOS.

## Harness changes worth knowing

- `storecorrupts.py` asked that a filled store have at least two packs. Under LLVM on
  Windows it had one, twice running, with every record in it. A collection makes one pack
  of the small ones once there are more than sixteen, and a local x86-64 fill leaves
  thirteen, so the likeliest reading is that the Windows fill went over. That is inferred:
  the count before the collection was not printed there. The harness now takes one pack.
- `breakers.py`'s `list_by_address` declares `vsnprintf`, which MSVC has out of line only
  in `legacy_stdio_definitions`; the case links it there.
- `backends.py` ran on nothing but Unix (`os.wait4`, `resource`, the platform's encoding
  for what blaze prints); on Windows it now runs, without the peak memory.
