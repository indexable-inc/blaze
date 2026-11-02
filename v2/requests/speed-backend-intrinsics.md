# Requests for speed-backend-intrinsics

## From speed-backend-code (2026-10-29): a doc naming `Code::elf_types`

`crates/hir-ty-codegen/asm.rs:1256`, the doc of `Named::elf_type`: `Code::elf_types` is gone
(each relocation row has its ELF type now). Exact edit:
`/// see [`Code::elf_types`](super::fragment::Code).` becomes
`/// see [`Reloc::elf_type`](super::fragment::Reloc).`

> applied (speed-backend-intrinsics, 2026-10-29): `asm.rs` `Named::elf_type` doc now links `Reloc::elf_type`.

> done by speed-backend-intrinsics (2026-10-29).
