// blaze (vendor/corosensei/SOURCE.md): src/arch/x86_64.rs's two `global_asm!` blocks, expanded by
// hand for ELF (no symbol prefix, `.type`/`.size` as asm_function_begin!/asm_function_end!,
// `.cfi_signal_frame`) and assembled by build.rs. Upstream writes Intel syntax; this is the same
// instructions in AT&T syntax, which every assembler reads alike (Intel `0f` label references and
// unsized `jmp [mem]` are not). CFI is upstream's. See x86_64.rs for what each line does.

    .text

    .balign 16
    .globl stack_init_trampoline
    .type stack_init_trampoline, @function
stack_init_trampoline:
    .cfi_startproc
    .cfi_signal_frame
    pushq %rbp                    // push rbp
    movq %rsp, -16(%rsi)          // mov [rsi - 16], rsp
    subq $16, %rsi                // sub rsi, 16
    leaq 8(%rdx), %rsp            // lea rsp, [rdx + 8]
    movq %rsi, %rbp               // mov rbp, rsi
    // DW_CFA_def_cfa_expression, 5 bytes: DW_OP_breg6 0, DW_OP_deref, DW_OP_plus_uconst 24.
    .cfi_escape 0x0f, 5, 0x76, 0x00, 0x06, 0x23, 0x18
    .cfi_offset %rbx, -8
    .cfi_offset %rip, -16
    .cfi_offset %rbp, -24
    movq %rsp, %rdx               // mov rdx, rsp
    leaq 0f(%rip), %rcx           // lea rcx, [rip + 0f]
    pushq %rcx                    // push rcx
    jmpq *8(%rsi)                 // jmp [rsi + 8]
0:
    .globl stack_init_trampoline_return
    .type stack_init_trampoline_return, @function
stack_init_trampoline_return:
    int3
    .cfi_endproc
    .size stack_init_trampoline, . - stack_init_trampoline

    .balign 16
    .globl stack_call_trampoline
    .type stack_call_trampoline, @function
stack_call_trampoline:
    .cfi_startproc
    .cfi_signal_frame
    pushq %rbp                    // push rbp
    movq %rsp, %rbp               // mov rbp, rsp
    .cfi_def_cfa %rbp, 16
    .cfi_offset %rbp, -16
    movq %rsi, %rsp               // mov rsp, rsi
    callq *%rdx                   // call rdx
    movq %rbp, %rsp               // mov rsp, rbp
    popq %rbp                     // pop rbp
    retq                          // ret
    .cfi_endproc
    .size stack_call_trampoline, . - stack_call_trampoline

    // A non-executable stack, as rustc's own objects say.
    .section .note.GNU-stack, "", @progbits
