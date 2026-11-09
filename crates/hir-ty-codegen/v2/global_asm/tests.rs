//! A crate's `global_asm!`s, over fixtures: corosensei's trampolines, written as corosensei
//! 0.3.4 writes them (its `asm_function_begin!`, `cfi!` and `seh!` macros, `concat!`), and
//! blocks blaze refuses.

#[path = "../tests/db.rs"]
mod db;

use hir_expand::fixture;
use object::{Object, ObjectSection, ObjectSymbol};

use super::*;
use crate::v2::{BuildTime, Databases, HashSource, ItemHashes};
use db::TestDB;

fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    let hashes = HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    (db, HirFrontend::new(Databases::new(db), hashes, BuildTime::default()))
}

/// The builtin macros a crate's `global_asm!` and corosensei's own reach.
const BUILTINS: &str = r#"
#[rustc_builtin_macro]
macro_rules! global_asm {() => {}}
#[rustc_builtin_macro]
macro_rules! concat {() => {}}
macro_rules! seh { ($asm:expr) => { "" } }
macro_rules! cfi { ($asm:expr) => { $asm } }
"#;

/// corosensei's `src/arch/mod.rs` for Mach-O.
const MACH_O: &str = r#"
macro_rules! asm_mangle { ($name:literal) => { concat!("_", $name) }; }
macro_rules! asm_function_begin {
    ($name:literal) => {
        concat!(
            ".globl ", asm_mangle!($name), "\n",
            ".private_extern ", asm_mangle!($name), "\n",
            asm_mangle!($name), ":\n",
        )
    };
}
macro_rules! asm_function_alt_entry {
    ($name:literal) => { concat!(".alt_entry ", asm_mangle!($name), "\n", asm_function_begin!($name)) };
}
macro_rules! asm_function_end { ($name:literal) => { "" }; }
macro_rules! cfi_signal_frame { () => { "" } }
"#;

/// corosensei's `src/arch/mod.rs` for ELF.
const ELF: &str = r#"
macro_rules! asm_mangle { ($name:literal) => { $name }; }
macro_rules! asm_function_begin {
    ($name:literal) => {
        concat!(
            ".globl ", asm_mangle!($name), "\n",
            ".type ", asm_mangle!($name), ", ", "@function", "\n",
            asm_mangle!($name), ":\n",
        )
    };
}
macro_rules! asm_function_alt_entry { ($name:literal) => { asm_function_begin!($name) }; }
macro_rules! asm_function_end {
    ($name:literal) => { concat!(".size ", asm_mangle!($name), ", . - ", asm_mangle!($name), "\n") };
}
macro_rules! cfi_signal_frame { () => { ".cfi_signal_frame" } }
"#;

/// corosensei's `src/arch/aarch64.rs` trampolines, its comments left out.
const AARCH64: &str = r#"
global_asm!(
    ".balign 4",
    asm_function_begin!("stack_init_trampoline"),
    cfi!(".cfi_startproc"),
    seh!(".seh_proc stack_init_trampoline"),
    cfi!(cfi_signal_frame!()),
    "stp x29, lr, [sp, #-32]!",
    "str x19, [sp, #16]",
    "mov x3, sp",
    "str x3, [x1, #-16]!",
    "add sp, x2, #32",
    "mov x29, x1",
    seh!(".seh_save_fplr_x 32"),
    seh!(".seh_endprologue"),
    cfi!(".cfi_escape 0x0f, 5, 0x8d, 0x00, 0x06, 0x23, 0x20"),
    cfi!(".cfi_offset x19, -16"),
    cfi!(".cfi_offset lr, -24"),
    cfi!(".cfi_offset x29, -32"),
    "mov x2, sp",
    "adr lr, 0f",
    "ldr x3, [x1, #8]",
    "br x3",
    "0:",
    asm_function_alt_entry!("stack_init_trampoline_return"),
    "brk #0",
    seh!(".seh_endproc"),
    cfi!(".cfi_endproc"),
    asm_function_end!("stack_init_trampoline"),
);

global_asm!(
    ".balign 4",
    asm_function_begin!("stack_call_trampoline"),
    cfi!(".cfi_startproc"),
    seh!(".seh_proc stack_call_trampoline"),
    cfi!(cfi_signal_frame!()),
    "stp x29, x30, [sp, #-16]!",
    "mov x29, sp",
    cfi!(".cfi_def_cfa x29, 16"),
    cfi!(".cfi_offset x30, -8"),
    cfi!(".cfi_offset x29, -16"),
    "mov sp, x1",
    "blr x2",
    "mov sp, x29",
    "ldp x29, x30, [sp], #16",
    "ret",
    seh!(".seh_endproc"),
    cfi!(".cfi_endproc"),
    asm_function_end!("stack_call_trampoline"),
);
"#;

/// corosensei's `src/arch/x86_64.rs` trampolines, its comments left out.
const X86_64: &str = r#"
global_asm!(
    ".balign 16",
    asm_function_begin!("stack_init_trampoline"),
    cfi!(".cfi_startproc"),
    seh!(".seh_proc stack_init_trampoline"),
    cfi!(cfi_signal_frame!()),
    "push rbp",
    "mov [rsi - 16], rsp",
    "sub rsi, 16",
    "lea rsp, [rdx + 8]",
    "mov rbp, rsi",
    cfi!(".cfi_escape 0x0f, 5, 0x76, 0x00, 0x06, 0x23, 0x18"),
    cfi!(".cfi_offset rbx, -8"),
    cfi!(".cfi_offset rip, -16"),
    cfi!(".cfi_offset rbp, -24"),
    seh!(".seh_endprologue"),
    "mov rdx, rsp",
    "lea rcx, [rip + 0f]",
    "push rcx",
    "jmp [rsi + 8]",
    "0:",
    asm_function_alt_entry!("stack_init_trampoline_return"),
    "int3",
    cfi!(".cfi_endproc"),
    seh!(".seh_endproc"),
    asm_function_end!("stack_init_trampoline"),
);

global_asm!(
    ".balign 16",
    asm_function_begin!("stack_call_trampoline"),
    cfi!(".cfi_startproc"),
    seh!(".seh_proc stack_call_trampoline"),
    cfi!(cfi_signal_frame!()),
    "push rbp",
    "mov rbp, rsp",
    cfi!(".cfi_def_cfa rbp, 16"),
    cfi!(".cfi_offset rbp, -16"),
    "mov rsp, rsi",
    "call rdx",
    "mov rsp, rbp",
    "pop rbp",
    "ret",
    cfi!(".cfi_endproc"),
    seh!(".seh_endproc"),
    asm_function_end!("stack_call_trampoline"),
);
"#;

fn assembled(triple: &str, text: &str) -> Result<Option<GlobalAsmObject>, String> {
    let (db, frontend) = frontend(text);
    frontend.global_asm(fixture::test_crate(db), triple).map_err(|it| it.what)
}

/// The `global_asm!`s corosensei's coroutines switch stacks with assemble to one object that
/// defines each trampoline, global and in code, with the unwind entries its `.cfi_*`s say:
/// the symbols `blaze-graph`'s scheduler left undefined when blaze dropped them.
#[test]
#[cfg(feature = "llvm")]
fn corosenseis_trampolines_assemble_to_an_object_defining_them() {
    for (triple, format, arch, prefix, unwind) in [
        ("aarch64-apple-darwin", MACH_O, AARCH64, "_", &["__compact_unwind", "__eh_frame"][..]),
        ("x86_64-unknown-linux-gnu", ELF, X86_64, "", &[".eh_frame"][..]),
    ] {
        let text = format!("//- /lib.rs crate:corosensei\n{BUILTINS}{format}{arch}");
        let object = assembled(triple, &text).unwrap_or_else(|e| panic!("{triple}: {e}"));
        let object = object.unwrap_or_else(|| panic!("{triple}: no object of two `global_asm!`s"));
        assert!(object.name.contains("crate `corosensei`"), "{triple}: {}", object.name);
        let file = object::File::parse(&*object.bytes).unwrap();
        for name in
            ["stack_init_trampoline", "stack_init_trampoline_return", "stack_call_trampoline"]
        {
            let symbol = file
                .symbols()
                .find(|it| it.name() == Ok(&format!("{prefix}{name}")))
                .unwrap_or_else(|| panic!("{triple}: no symbol `{prefix}{name}`"));
            assert!(
                symbol.is_definition() && symbol.is_global(),
                "{triple}: `{name}` is not defined global"
            );
            let section = file.section_by_index(symbol.section_index().unwrap()).unwrap();
            assert_eq!(section.kind(), object::SectionKind::Text, "{triple}: `{name}` is not code");
        }
        assert!(
            file.sections().any(|it| unwind.contains(&it.name().unwrap_or("")) && it.size() > 0),
            "{triple}: no unwind entries of the `.cfi_*`s"
        );
        let again = assembled(triple, &text).unwrap().unwrap();
        assert_eq!(
            (again.id, &*again.bytes),
            (object.id, &*object.bytes),
            "{triple}: built twice, other bytes"
        );
    }
}

/// The line of `text`'s file, from 1, that is `line`.
fn line_of(text: &str, line: &str) -> usize {
    let file = text.split_once('\n').unwrap().1;
    file.lines().position(|it| it == line).unwrap() + 1
}

/// A directive the assembler does not know is an error naming the crate, the file and line
/// of the `global_asm!`, the line of its template and the directive, never an object without it.
#[test]
#[cfg(feature = "llvm")]
fn a_directive_blaze_cannot_assemble_is_an_error_naming_it() {
    for triple in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"] {
        let text = format!(
            "//- /lib.rs crate:refused\n{BUILTINS}\nglobal_asm!(\".balign 4\");\n\
             global_asm!(\n    \"nop\",\n    \".blaze_no_such_directive 1\",\n);\n"
        );
        let error = assembled(triple, &text).unwrap_err();
        let line = line_of(&text, "global_asm!(");
        for said in [
            &format!("the `global_asm!` at lib.rs:{line} of crate `refused`")[..],
            "line 2 of its template",
            "`.blaze_no_such_directive 1`",
        ] {
            assert!(error.contains(said), "{triple}: says {said:?}: {error}");
        }
        let first = line_of(&text, "global_asm!(\".balign 4\");");
        assert!(
            !error.contains(&format!("lib.rs:{first} ")),
            "{triple}: names the block that has it alone: {error}"
        );
    }
}

/// What a `global_asm!` has that blaze assembles nothing of is an error naming it.
#[test]
fn an_operand_or_a_wasm_target_is_an_error_naming_the_block() {
    let text =
        format!("//- /lib.rs crate:operands\n{BUILTINS}\nglobal_asm!(\".byte {{}}\", const 5);\n");
    let error = assembled("x86_64-unknown-linux-gnu", &text).unwrap_err();
    assert!(error.contains("of crate `operands` has a `const` operand"), "{error}");
    let text = format!("//- /lib.rs crate:wasm\n{BUILTINS}\nglobal_asm!(\".globl x\");\n");
    let error = assembled("wasm32-unknown-unknown", &text).unwrap_err();
    assert!(error.contains("of crate `wasm`") && error.contains("wasm32"), "{error}");
}

/// A crate with no `global_asm!` gives the link nothing.
#[test]
fn a_crate_without_global_asm_has_no_object() {
    let text = format!("//- /lib.rs crate:none\n{BUILTINS}\npub fn f() {{}}\n");
    assert_eq!(assembled("aarch64-apple-darwin", &text), Ok(None));
}

/// LLVM's assembler's error, read for the lines it names.
#[test]
fn an_assemblers_lines_are_read_from_its_error() {
    let llvm = "inline assembly: <inline asm>:4:1: error: unknown directive\n.foo\n^; <inline asm>:9:3: invalid operand";
    assert_eq!(diagnostics(llvm), [(4, "unknown directive"), (9, "invalid operand")]);
    assert!(diagnostics("inline assembly: nothing here assembles it").is_empty());
}

/// A blaze with no assembler (neither LLVM nor rsasm) assembles no `global_asm!`: it says so,
/// naming the block, and never hands the text to the machine's C compiler
/// (docs/v2/plan/design/toolchain-c.md, slice 1).
#[test]
#[cfg(not(any(feature = "llvm", feature = "rsasm")))]
fn without_llvm_a_global_asm_is_an_error_not_the_machines_compiler() {
    let text = format!("//- /lib.rs crate:nollvm\n{BUILTINS}\nglobal_asm!(\"nop\");\n");
    let error = assembled("x86_64-unknown-linux-gnu", &text).unwrap_err();
    for said in [
        "of crate `nollvm`",
        "built with no assembler",
        "blaze-engine's `native`",
        "hir-ty-codegen's `rsasm`",
    ] {
        assert!(error.contains(said), "says {said:?}: {error}");
    }
}
