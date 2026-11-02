# Requests from fastcompile-stencil-diff-clif-simd

## 1. `avg_round` has no file: the interpreter panics on it

- What: the brief asks for `avg_round` of `i8x16`/`i16x8` in `simd-arithmetic.clif` (and of
  `i32x4` in `simd-extra.clif`). It is left out of both.
- Where: `cranelift/interpreter/src/step.rs` (the indexable fork, which matches 0.136 here):
  `Opcode::AvgRound` applies scalar `DataValueExt::add` to the whole `V128`, which reaches
  `binary_match!`'s `unimplemented!()`. `crates/blaze-stencil/src/tests/interp.rs`'s `run`
  doesn't catch panics, so one such function fails
  `every_run_line_holds_in_the_interpreter` for the whole file.
- Why it matters: `avg_round` is in the stencil set (`shape.rs`, `INT_VECTORS`) and wasm has
  it (`i8x16.avgr_u`, `i16x8.avgr_u`), so it should be tested against Cranelift's code.
- Ask: either fix the interpreter's vector `avg_round` (lane by lane, `(a + b + 1) >> 1`
  unsigned, in the double width), or let `clif.rs` skip the interpreter for a file or function
  marked to need it (and add the file below to `FILES`). Text ready to place, as
  `simd-avg-round.clif` or appended to `simd-arithmetic.clif`:

```
function %avg_round_i8x16(i8x16, i8x16) -> i8x16 {
block0(v0: i8x16, v1: i8x16):
    v2 = avg_round v0, v1
    return v2
}
; run: %avg_round_i8x16([0 1 0xff 0xff 0x80 0x7f 2 3 0 0 0 0 0 0 0 0], [0 0 0xff 0xfe 0x7f 0x80 5 4 0 0 0 0 0 0 0 0]) == [0 1 0xff 0xff 0x80 0x80 4 4 0 0 0 0 0 0 0 0]

function %avg_round_i16x8(i16x8, i16x8) -> i16x8 {
block0(v0: i16x8, v1: i16x8):
    v2 = avg_round v0, v1
    return v2
}
; run: %avg_round_i16x8([0 1 0xffff 0xffff 0x8000 0x7fff 2 3], [0 0 0xffff 0xfffe 0x7fff 0x8000 5 4]) == [0 1 0xffff 0xffff 0x8000 0x8000 4 4]
```

> forwarded to fastcompile (from requests, pass 17; told by tmux).

## 2. Two lanes own the same eight files

- What: my brief (`briefs/v2/fastcompile-stencil-diff-clif-simd.md`) still gives me all 16
  `simd-*.clif` files, but `fastcompile-stencil-diff-clif-simd2` was split off with
  `simd-shuffle`, `simd-splat`, `simd-swizzle`, `simd-valltrue`, `simd-vanytrue`,
  `simd-vhighbits`, `simd-widen` and `simd-extra`, and has already overwritten my
  `simd-shuffle.clif`.
- What I did: stopped writing those eight; they are `simd2`'s. My drafts of seven of them
  (all but shuffle, which `simd2` replaced) are in
  `/tmp/blaze-lanes/v2-fastcompile-stencil-diff-clif-simd-drafts/` for `simd2` to take from
  or discard; the copies in the worktree will be overwritten as `simd2` reaches them.
- Ask: correct my brief's ownership list to the first eight files, so the next reader of it
  doesn't take the other eight back.

> forwarded to fastcompile (from requests, pass 17; told by tmux).
