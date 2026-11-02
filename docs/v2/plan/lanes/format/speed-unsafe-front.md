---
status: plan
summary: Index of speed-unsafe-front's unsafe.md pass over lane format's crates (syntax-tree, parser, syntax, span, intern, tt, mbe, syntax-bridge, const-ast, hir-def's SoA files); the findings are in the children's notes and db/speed-unsafe-front.md.
date: 2026-10-30
---
# speed-unsafe-front: lane format's crates under unsafe.md

The pass and its shared findings (how `unsafe_code` treats macros, hir-def's ids) are in
[../db/speed-unsafe-front.md](../db/speed-unsafe-front.md). Lane format's crates were done by
two children:

- `intern`, `span`, `tt`, `mbe`, `syntax-bridge`, `const-ast`:
  [speed-unsafe-front-intern.md](speed-unsafe-front-intern.md).
- `syntax-tree`, `parser`, `syntax`: [speed-unsafe-front-syntax.md](speed-unsafe-front-syntax.md).
- hir-def's `expr_store*`, `hir*` and `item_tree*` (format's files in hir-def), with the rest of
  hir-def: [../db/speed-unsafe-front-def.md](../db/speed-unsafe-front-def.md).
