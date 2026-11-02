# rustc_ast_ir

- Upstream: crates.io `ra-ap-rustc_ast_ir` 0.179.0, rust-analyzer's publication of rustc's
  `compiler/rustc_ast_ir` at rust-lang/rust `dc44e7f2c436b6041742e138b9c2176f093196d5`.
- Licence: MIT or Apache-2.0, as upstream.
- Left out: Cargo.lock. `Cargo.toml` and `Cargo.toml.orig` are the published ones.
- The root manifest patches crates.io's copy with this one, so `rustc_type_ir`, hir-ty and the
  rest share one `IntTy` and `UintTy`.

Changes, marked `blaze:` in src/lib.rs:

1. The integer types of blaze's extension `#![blaze::arbitrary_ints]` (`u1`..`u127`,
   `i1`..`i127`; docs/v2/plan/lanes/frontend/arbitrary-ints.md): `IntWidth` (1 to 127 bits,
   never a standard width, so `u8` keeps one spelling) and the variants `IntTy::Bits` and
   `UintTy::Bits`, with `of_width`, `storage` (the standard type a width is stored as) and
   `arbitrary_width`. `bit_width` is the value's width (3 for `u3`), never its storage's.
   A variant rather than a new type kind, so every exhaustive match over the integer kinds has
   to say what a width means for it, and code that matches `TyKind::Uint(_)` (builtin
   operators, casts, `is_integral`) takes them as the integers they are.
