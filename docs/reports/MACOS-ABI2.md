---
status: record
summary: macOS arm64: the remaining ABI and backend gaps.
date: 2026-10-20
---
# macOS arm64: the remaining ABI and backend gaps

Branch `blaze-macos-abi2`, cut from `blaze-collate` at 6d12f21c86. Everything under
"Measured" ran natively on an Apple Silicon Mac (`uname -m` is `arm64`, stable
1.99.0-aarch64-apple-darwin, LLVM 20.1.8 from Homebrew) or in GitHub Actions as stated.
Nothing was emulated except the one x86_64-apple-darwin attempt, which did not get as far
as running.

## What changed

### 128-bit and 16-byte-aligned arguments that go to the stack

A 128-bit integer that the argument registers left do not hold used to be refused, which
left `cases::foreign::wides` to rustc. It now goes to the stack whole, the registers it did
not fit in go out of use, and the integer arguments after it are on the stack too. The same
goes for an aggregate aligned to sixteen bytes, which is on the stack at a multiple of
sixteen. `PassMode::Uniform` carries it: the skipped registers are filled, and the value is
one `I128` unit so that Cranelift and LLVM both place it at a multiple of sixteen. Both
backends take this from the same classification in `aapcs64_abi`, so there is no second
copy for LLVM.

New corpus cases: `foreign::aligned` and `foreign::wide_last`; `foreign::wides` is now
compiled by blaze.

### Variadic definitions on Apple arm64

- Named parameters on the stack. The `VaList` used to start at the frame pointer plus 16,
  which is only right when every named parameter is in a register. `Variadic::apple_stack`
  now counts the bytes of stack the named parameters take (each at a multiple of its own
  size, 128-bit integers and pairs included, with a register kind out of use once one of
  its parameters has gone to the stack) and the list starts after them.
- `va_arg` of a 128-bit integer: read at the next multiple of sixteen, advance by sixteen.
- `va_arg` of a struct is not something a Rust program can write. `VaList::next_arg` takes
  `T: VaArgSafe`, and that trait cannot be implemented outside `core` ("trait cannot be
  implemented outside `core`", tried with rustc 1.99). So there is no case for it and no
  code for it; a `va_arg` of anything that is not one scalar is refused as before.

New corpus cases: `stabilized::c_variadic_named_stack`, `c_variadic_named_wide` (Apple
only, since blaze refuses such a definition elsewhere), and `c_variadic_wide` behind the
corpus's `nightly` feature (128-bit `va_arg` needs it).

### `sym` operands of `naked_asm!`

MIR lowering resolves a `sym` to a static or to a function without generic parameters
(`AsmOperand::Function`, `AsmOperand::Static`); anything else is refused. The text of the
function names the operand `blaze_asm_sym_N`, the assembler leaves a relocation against
that name, and `asm::code` turns each one into a relocation against the function or the
static the operand names. Branches, page/page-offset pairs and GOT loads are translated for
Mach-O arm64; the ELF arm64 and ELF x86-64 kinds are in the same table but have never been
executed. A naked function with a `sym` is assembled every time rather than kept in the
store, which holds code alone.

`sym` in an ordinary `asm!` block is still refused ("an `asm!` block with a `sym`"): the
function of a block is shared by its text, which does not say what the name stands for.

New corpus case: `stabilized::arm::naked_sym` (a jump to a function, a call of one, a
static read through `@PAGE`/`@PAGEOFF` with and without an addend).

### CI

`.github/workflows/blaze.yaml` has a `macos` job on `macos-latest` for both backends: fmt
and the tests (once, in the Cranelift leg), the build with `--features llvm`, then corpus
with `--layouts`, programs, stress, breakers, unwinds, splices, asmedits, deps, includes,
worktrees in its three modes, and keys. `deps.py` is not in it, as it is not in the Linux
job: it builds offline from cargo's registry cache, which a runner does not have ("failed
to download `aho-corasick v1.1.5` ... --offline was specified" on the first run). Both
harness steps now print which harnesses failed
as the last line of the log.

## Measured

Local, native, on the head commit's code. LLVM is `BLAZE_BACKEND=llvm BLAZE_OPT=2
BLAZE_EMIT=fast`.

| harness | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --layouts` | MATCH, 816/816 from blaze; layouts 0 of 913 differ | MATCH, 816/816; 0 of 913 |
| corpus with the `nightly` feature | MATCH, 825/825 | MATCH, 825/825 |
| `programs.py` | 22/22 | 22/22 |
| `stress.py` | 99/99, 0 restarts | 99/99, 0 restarts |
| `breakers.py` | 25/25, 0 open, 0 skipped | 25/25, 0 open, 0 skipped |
| `unwinds.py` | 8/8 | 8/8 |
| `BLAZE_SPLICE=1 splices.py` | 71/71 | 71/71 |
| `asmedits.py` | 10/10 | 10/10 |
| `deps.py` | 10/10 | 10/10 |
| `includes.py` | 9/9 | 9/9 |
| `worktrees.py` | 58/58 | 58/58 |
| `worktrees.py --join --together` | 58/58 | 58/58 |
| `worktrees.py --overlay` | 67/67 | 67/67 |
| `keys.py` | 26/26, 4 open | 26/26, 4 open |
| `scoreboard.py` | all targets match (corpus 816/816) | not run |

No function of the corpus is left to rustc. Under LLVM, `backends.py` on the corpus says 3
of 11,851 functions are left to Cranelift, all for "more than two return values". The
daemon logs of the LLVM worktrees and includes runs have `backend: llvm -O2 fast emit` and
no fallback.

The harnesses that had not been run under LLVM before (worktrees in three modes, asmedits,
includes, and also deps and keys) passed on the first run; nothing needed fixing.

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine` passes. `cargo clippy -p blaze-engine -p hir-ty --all-targets` has no errors.

GitHub Actions, on 8354d5f92f (the commits after it change this report only):

| job | result |
|---|---|
| macOS arm64 (cranelift), `macos-latest` (image macos-26-arm64) | green: fmt, tests, corpus MATCH 816/816, programs 22/22, stress 99/99, breakers 25/25, unwinds 8/8, splices 71/71, asmedits 10/10, includes 9/9, worktrees 58/58, 58/58, 67/67, keys 26/26 |
| macOS arm64 (llvm) | green, the same numbers |
| harness (cranelift), Linux x86-64 | green |
| harness (llvm), Linux x86-64 | green |
| fmt and tests, Linux | green |
| coverage (another workflow) | red, as on 6d12f21c86: `1033 passed; 18 failed` in one test binary on both (I compared the counts, not the names) |

So Linux x86-64 was run on this branch, by CI, not by me locally.

## A bug this found on Linux x86-64

The first push had the new `foreign::aligned` case on every target, and both Linux harness
jobs failed the corpus on it: blaze printed 37476700408053505959396591 where rustc prints
37476700408053504415892978. The function was compiled by blaze (806/806) and no code of
this branch is on that path, so this is a bug that was there: on x86-64 an aggregate
aligned to sixteen bytes that goes to the stack is a Cranelift `StructArgument`, which
Cranelift places at the next multiple of eight. The case has three such arguments; the
second or third lands eight bytes off. I did not fix it (not my platform, and I cannot run
Linux here); the case is `#[cfg(target_arch = "aarch64")]` with a FIXME saying so. On
AArch64 the same situation is what this round fixed.

## x86_64-apple-darwin

Tried locally under Rosetta 2, not on a runner. `Target::parse` refuses the triple today.
With one line added to accept it as Mach-O with the System V convention (not committed),
blaze compiled the whole corpus crate and the link failed:

    ld: __eh_frame unexpected relocation type (1) at r_address=0x00065DA9

So what is known to be missing is the unwind table: ld wants the PC-relative fields of
`__eh_frame` on x86-64 Mach-O as SUBTRACTOR/UNSIGNED pairs (and the personality through the
GOT), and `unwind.rs` writes them as `X86_64_RELOC_SIGNED`, which the `object` crate's
writer has no pair form for. Nothing after the link has been seen, so this is a lower bound.
By reading the code, these are also missing: x86-64 Mach-O relocations in
`llvm/emitted.rs`, the splice (`splice/macho.rs` is arm64 only), and TLS for that pairing.

## Shared code to recheck on Linux

- `aapcs64_abi` in `codegen.rs`: the overflow branch for a 128-bit integer and for an
  aggregate aligned to sixteen bytes. This is reached on Linux aarch64 too, where it was a
  refusal before and is now code that has not run there.
- `PassMode::Uniform` with an `I128` unit, in the parameter list and where the values are
  loaded.
- The `va_arg` arm: the homed (Windows) case still refuses anything wider than eight
  bytes; the System V arm is untouched.
- `asm.rs`: `code()` now returns relocations instead of refusing them, and the two callers
  that keep code in the store refuse them as before. `Assembler::assemble` returns the
  object and the caller extracts the code.
- `mir.rs`, `lower.rs`, `monomorphization.rs`: two new `AsmOperand` variants. A `sym` in a
  naked function is now accepted on every target; the relocation table decides whether the
  use of it is one blaze knows.

## What is left

0. The x86-64 stack placement of 16-byte-aligned aggregates above (Linux, and Windows should
   be checked).
1. `sym` in an ordinary `asm!` block.
2. `va_arg` of a struct, if Rust ever allows it.
3. x86_64-apple-darwin, starting with the unwind table.
4. The ELF rows of the `sym` relocation table have never run.
