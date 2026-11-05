// The object of blaze-link's `macho_unwind` test: `_main` calls `_middle(_leaf)`, `_leaf`
// walks the stack with `_Unwind_Backtrace`, and `_seen` notes which of `_middle` and `_main`
// the walk passes. `_middle` keeps its CFA in x19 and leaves x29 alone, which compact unwind
// can't say: the assembler describes it with an `__eh_frame` FDE whose pc_begin is a
// SUBTRACTOR (`ltmp` at the start of `__eh_frame`) and UNSIGNED (`_middle`) pair, and the
// walk reaches `_main` only through that FDE. `_main` returns 0 when the walk saw both.
//
// Built with `clang -c -arch arm64 -mmacosx-version-min=11.0 eh_fde.s -o eh_fde.o`.
	.text
	.globl	_middle
	.p2align	2
_middle:
	.cfi_startproc
	stp	x29, x30, [sp, #-32]!
	.cfi_def_cfa_offset 32
	.cfi_offset w30, -24
	.cfi_offset w29, -32
	str	x19, [sp, #16]
	.cfi_offset w19, -16
	mov	x19, sp
	.cfi_def_cfa_register w19
	sub	sp, sp, #256
	blr	x0
	mov	sp, x19
	.cfi_def_cfa_register wsp
	ldr	x19, [sp, #16]
	ldp	x29, x30, [sp], #32
	.cfi_def_cfa_offset 0
	.cfi_restore w19
	.cfi_restore w30
	.cfi_restore w29
	ret
	.cfi_endproc

	.globl	_main
	.p2align	2
_main:
	.cfi_startproc
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	adrp	x0, _leaf@PAGE
	add	x0, x0, _leaf@PAGEOFF
	bl	_middle
	ldp	x29, x30, [sp], #16
	ret
	.cfi_endproc

	.globl	_leaf
	.p2align	2
_leaf:
	.cfi_startproc
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	adrp	x1, _state@PAGE
	add	x1, x1, _state@PAGEOFF
	str	xzr, [x1]
	adrp	x0, _seen@PAGE
	add	x0, x0, _seen@PAGEOFF
	bl	__Unwind_Backtrace
	adrp	x1, _state@PAGE
	add	x1, x1, _state@PAGEOFF
	ldr	x0, [x1]
	eor	x0, x0, #3
	ldp	x29, x30, [sp], #16
	ret
	.cfi_endproc

// `_Unwind_Trace_Fn`: x0 the context, x1 `&_state`. Bit 0 for an IP in `_middle`
// (`[_middle, _main)`), bit 1 for one in `_main` (`[_main, _leaf)`).
	.globl	_seen
	.p2align	2
_seen:
	.cfi_startproc
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	stp	x20, x19, [sp, #-16]!
	.cfi_offset w19, -24
	.cfi_offset w20, -32
	mov	x19, x1
	bl	__Unwind_GetIP
	adrp	x8, _middle@PAGE
	add	x8, x8, _middle@PAGEOFF
	adrp	x9, _main@PAGE
	add	x9, x9, _main@PAGEOFF
	adrp	x10, _leaf@PAGE
	add	x10, x10, _leaf@PAGEOFF
	ldr	x12, [x19]
	sub	x11, x0, x8
	sub	x13, x9, x8
	cmp	x11, x13
	b.hs	1f
	orr	x12, x12, #1
1:
	sub	x11, x0, x9
	sub	x13, x10, x9
	cmp	x11, x13
	b.hs	2f
	orr	x12, x12, #2
2:
	str	x12, [x19]
	mov	w0, #0
	ldp	x20, x19, [sp], #16
	ldp	x29, x30, [sp], #16
	ret
	.cfi_endproc

	.data
	.p2align	3
_state:
	.quad	0
