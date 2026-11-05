// blaze (vendor/corosensei/SOURCE.md): src/arch/aarch64.rs's two `global_asm!` blocks, expanded
// by hand for ELF (no symbol prefix, `.type`/`.size` as asm_function_begin!/asm_function_end!,
// `.cfi_signal_frame`) and assembled by build.rs. Instructions and CFI are upstream's text, except
// `%function` for `@function` and `x30` for `lr` in `.cfi_offset` (the same symbol type and DWARF
// register 30), spellings GNU as and LLVM both take; see aarch64.rs for what each line does.

    .text

    .balign 4
    .globl stack_init_trampoline
    .type stack_init_trampoline, %function
stack_init_trampoline:
    .cfi_startproc
    .cfi_signal_frame
    stp x29, lr, [sp, #-32]!
    str x19, [sp, #16]
    mov x3, sp
    str x3, [x1, #-16]!
    add sp, x2, #32
    mov x29, x1
    // DW_CFA_def_cfa_expression, 5 bytes: DW_OP_breg29 0, DW_OP_deref, DW_OP_plus_uconst 32.
    .cfi_escape 0x0f, 5, 0x8d, 0x00, 0x06, 0x23, 0x20
    .cfi_offset x19, -16
    .cfi_offset x30, -24
    .cfi_offset x29, -32
    mov x2, sp
    adr lr, 0f
    ldr x3, [x1, #8]
    br x3
0:
    .globl stack_init_trampoline_return
    .type stack_init_trampoline_return, %function
stack_init_trampoline_return:
    brk #0
    .cfi_endproc
    .size stack_init_trampoline, . - stack_init_trampoline

    .balign 4
    .globl stack_call_trampoline
    .type stack_call_trampoline, %function
stack_call_trampoline:
    .cfi_startproc
    .cfi_signal_frame
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    .cfi_def_cfa x29, 16
    .cfi_offset x30, -8
    .cfi_offset x29, -16
    mov sp, x1
    blr x2
    mov sp, x29
    ldp x29, x30, [sp], #16
    ret
    .cfi_endproc
    .size stack_call_trampoline, . - stack_call_trampoline

    // A non-executable stack, as rustc's own objects say.
    .section .note.GNU-stack, "", %progbits
