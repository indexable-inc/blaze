# rsasm

- Upstream: https://github.com/KarpelesLab/rsasm, master at
  19bdabcc946f716dfc35465e8b6d586b6d9b45e6 (2026-10-06), which still calls itself 0.1.3. The
  crates.io 0.1.3 (2026-09-21) has no aarch64 exclusive loads and stores (`ldxr`, `stlxr` and
  their byte, half and pair forms), no LSE atomics (`cas`, `casp`, `ldadd`, `ldclr`, `ldeor`,
  `ldset`, `swp` and their orderings) and no `.arch_extension`, which are what all 200 of
  compiler_builtins' outline atomics are written in; master has all three.
- Licence: MIT (LICENSE).
- Left out: `src/main.rs` and the `[[bin]]` and `[profile.release]` that build it (blaze uses
  the library), tests/, examples/, tools/, CHANGELOG.md.
- No changes to `src/`. Go back to the crates.io release once one past 0.1.3 has these.

blaze uses it as the seed's assembler: `crates/hir-ty-codegen/asm/rsasm.rs`,
todo/bootstrap/plan.md, "std and assembly in the seed".
