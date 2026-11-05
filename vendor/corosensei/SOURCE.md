# corosensei

- Upstream: https://github.com/Amanieu/corosensei, crates.io `corosensei` 0.3.4 (sha256
  6886a0c0f263965933c438626e7179139a62b978a33aa18281cbf0cd5a975f34, git
  4b5db4845204a4b2663d3f7f75866be4cc3c680d).
- Licence: MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE).
- Left out: `Cargo.toml.orig`, `Cargo.lock`, `.gitignore`, `.cargo_vcs_info.json`; `Cargo.toml` is
  the published one less its generated header.
- Excluded from the workspace (root `Cargo.toml`), like `vendor/wasmtime`: its tests and benches
  need dev-dependencies blaze has no use for.

## Why

This copy exists for one release, by the rule in the build-and-test skill (blaze-builds-blaze):
when stage 0 can't build the tree, write the source around it for one release, never cargo.

The pinned stage 0 (`[workspace.metadata.blaze] version` 0.20261030.1, built from 6560553 of
2026-10-29) predates blaze's `global_asm!` (8498b0dfc1, 2026-11-02,
`crates/hir-ty-codegen/v2/global_asm*.rs`). It drops corosensei's trampolines, so linking
blaze-engine fails with `Undefined symbols: _stack_init_trampoline, referenced from
_blaze::init_stack` (issue #1659).

## Changes

1. `src/arch/asm/{aarch64,x86_64}-{macho,elf}.S` hold the two trampolines (`stack_init_trampoline`,
   with its alternate entry `stack_init_trampoline_return`, and `stack_call_trampoline`) of
   `src/arch/aarch64.rs` and `src/arch/x86_64.rs`. Each is upstream's `global_asm!` expanded by hand
   through `src/arch/mod.rs`'s macros for that object format:
   - Mach-O: a `_` prefix, `.globl` plus `.private_extern`, `.alt_entry` on the return label, no
     `.cfi_signal_frame`, no `.size`, and `.subsections_via_symbols` as in LLVM's own Mach-O
     output (which is why upstream needs `.alt_entry`).
   - ELF: no prefix, `.globl` plus `.type`, `.cfi_signal_frame`, `.size`, and a
     `.note.GNU-stack` section so the stack stays non-executable.
   The instructions and CFI (`.cfi_escape` CFA expressions, `.cfi_offset`s, `.cfi_def_cfa`) are
   upstream's, except for spelling. x86_64 is written in AT&T syntax, not upstream's Intel syntax,
   because GNU as reads Intel-syntax `0f` label references and an unsized `jmp [mem]` differently.
   aarch64 ELF uses `%function` for `@function` and `x30` for `lr` in `.cfi_offset`: the same symbol
   type and DWARF register.
2. `build.rs` assembles the right file with `cc` (a new build-dependency) for the targets
   `src/arch/mod.rs` sends to those two files (x86_64 and aarch64, not Windows), except UEFI.
3. In `src/arch/aarch64.rs` and `src/arch/x86_64.rs` the `global_asm!` blocks and their import are
   `#[cfg(target_os = "uefi")]`. UEFI's SEH directives stay there, unchanged. The `extern "C"`
   declarations the Rust side calls through are unchanged.
4. `src/lib.rs` allows `unused_macros` outside UEFI: `asm_function_begin!` and the related macros
   now go unused on these targets.

Every other target (riscv, loongarch64, powerpc64, arm, x86, Windows) is as published.

## Removal

Delete this directory, its `[patch.crates-io]` line and its `exclude` entry in the root
`Cargo.toml` (and restore corosensei's registry entry in `Cargo.lock`) once stage 0 is a release
built after 8498b0dfc1, i.e. at the next stage-0 promotion. blaze's own `global_asm!` then assembles
upstream's trampolines; `crates/hir-ty-codegen/v2/global_asm/tests.rs` checks them.
