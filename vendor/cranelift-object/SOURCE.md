# cranelift-object

- Upstream: https://github.com/bytecodealliance/wasmtime (`cranelift/object`), crates.io
  `cranelift-object` 0.137.1 (sha256 c38304b64d440315410ae0229340dd2087bed7d7e8411a7bb72b4b6a2132db05; its `src/` is 0.137.0's byte for byte), taken from the crate with the changes below.
- Licence: Apache-2.0 WITH LLVM-exception (LICENSE).
- Left out: tests/ (and the dev-dependencies and `[[test]]` they need), Cargo.lock.

Changes, each marked `blaze:` in src/backend.rs (the fourth in src/lib.rs):

1. Data objects that name the same custom section (segment, name, kind and Mach-O type and
   attributes) share one section, as in LLVM's objects. Upstream adds a section for each, and a
   Mach-O object has room for 255: blaze writes a program as one object, so 255 statics with a
   `#[link_section]` across all its crates (`inventory::submit!` and `#[ctor]` each put one in
   `__DATA,__mod_init_func`) failed with `Too many sections` (#724). On ELF and COFF a data
   object keeps a section of its own where `per_data_object_section` or `#[used]` asks for one,
   as `add_subsection` does for the standard sections, since there the section is what the
   linker keeps or drops and what `SHF_GNU_RETAIN` is on. Mach-O objects go by symbols
   (`MH_SUBSECTIONS_VIA_SYMBOLS`, `N_NO_DEAD_STRIP`), so there they always share. A COFF
   object keeps upstream's section for each: a COFF symbol has no size, so the section is what
   says how large a static is (`Target::section_of_static`, read by the PE splice's `rooms`);
   shared, a static read as reaching the next one, alignment padding and all.
2. A zero-initialized data object in a custom section is written as zeros: the section is not
   one of uninitialized data, and another data object may follow it there.
3. A relocation the object format has no form for (`process_reloc`) is a `ModuleError` naming
   the relocation and the format, returned through `finish`, where upstream panics with
   `unimplemented!`.
4. `src/lib.rs` is `#![forbid(unsafe_code)]`.

Upstream 0.137 took what was blaze's third change (`used` marks a Mach-O symbol
`N_NO_DEAD_STRIP` once it is defined), so it is no longer one.
