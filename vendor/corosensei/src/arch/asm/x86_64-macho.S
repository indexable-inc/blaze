// blaze (vendor/corosensei/SOURCE.md): src/arch/x86_64.rs's two `global_asm!` blocks, expanded by
// hand for Mach-O (asm_mangle!'s `_`, asm_function_begin!'s `.private_extern`, an empty
// cfi_signal_frame!, an empty asm_function_end!) and assembled by build.rs. Upstream writes Intel
// syntax; this is the same instructions in AT&T syntax, which every assembler reads alike (Intel
// `0f` label references and unsized `jmp [mem]` are not). CFI is upstream's. See x86_64.rs for
// what each line does.

    .subsections_via_symbols
    .text

    .balign 16
    .globl _stack_init_trampoline
    .private_extern _stack_init_trampoline
_stack_init_trampoline:
    .cfi_startproc
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
    .alt_entry _stack_init_trampoline_return
    .globl _stack_init_trampoline_return
    .private_extern _stack_init_trampoline_return
_stack_init_trampoline_return:
    int3
    .cfi_endproc

    .balign 16
    .globl _stack_call_trampoline
    .private_extern _stack_call_trampoline
_stack_call_trampoline:
    .cfi_startproc
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
