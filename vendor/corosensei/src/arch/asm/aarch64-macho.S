// blaze (vendor/corosensei/SOURCE.md): src/arch/aarch64.rs's two `global_asm!` blocks, expanded
// by hand for Mach-O (asm_mangle!'s `_`, asm_function_begin!'s `.private_extern`, an empty
// cfi_signal_frame!, an empty asm_function_end!) and assembled by build.rs. Instructions and CFI
// are upstream's text; see aarch64.rs for what each line does.

    .subsections_via_symbols
    .text

    .balign 4
    .globl _stack_init_trampoline
    .private_extern _stack_init_trampoline
_stack_init_trampoline:
    .cfi_startproc
    stp x29, lr, [sp, #-32]!
    str x19, [sp, #16]
    mov x3, sp
    str x3, [x1, #-16]!
    add sp, x2, #32
    mov x29, x1
    // DW_CFA_def_cfa_expression, 5 bytes: DW_OP_breg29 0, DW_OP_deref, DW_OP_plus_uconst 32.
    .cfi_escape 0x0f, 5, 0x8d, 0x00, 0x06, 0x23, 0x20
    .cfi_offset x19, -16
    .cfi_offset lr, -24
    .cfi_offset x29, -32
    mov x2, sp
    adr lr, 0f
    ldr x3, [x1, #8]
    br x3
0:
    .alt_entry _stack_init_trampoline_return
    .globl _stack_init_trampoline_return
    .private_extern _stack_init_trampoline_return
_stack_init_trampoline_return:
    brk #0
    .cfi_endproc

    .balign 4
    .globl _stack_call_trampoline
    .private_extern _stack_call_trampoline
_stack_call_trampoline:
    .cfi_startproc
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
