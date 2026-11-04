//! The assembler of a blaze built without LLVM (the seed: docs/future/bootstrap.md, "std and
//! assembly in the seed"): rsasm, an assembler in Rust with no dependencies, run in this
//! process. It is a dependency of the code generator, so its version is part of the code
//! generator's identity (`v2::toolchain::CodegenCode`) and of every key over what it made.
//! Never the machine's C compiler or `as`.
//!
//! It is given what LLVM's assembler is given (`llvm::assemble`): the text in the syntax
//! `asm!` is written in, Intel's on x86-64 as rustc wraps a block. Its errors name lines as
//! LLVM's do (`<inline asm>:L:C: error: ...`, joined by `; `), which `v2::global_asm` reads
//! to name the line of a template.
//!
//! A build with LLVM assembles with LLVM's, so a process has one assembler. Making this the
//! one assembler of every build, checked against LLVM's on every block blaze's own build
//! assembles, is the next step (`docs/future/bootstrap.md`).

use std::fmt::Write;

use rsasm::{
    arch,
    assembler::{Assembler, Options},
    diag::Severity,
    output::{self, Format},
};

use crate::{Arch, ObjectFormat, Target};

/// What errors name the text by, as LLVM's do.
const NAME: &str = "<inline asm>";

/// The object file rsasm makes of `text` for `target`.
pub(super) fn assemble(target: &'static Target, text: &str) -> Result<Vec<u8>, String> {
    let (cpu, text) = match target.arch {
        // What rustc wraps `global_asm!` in, where there are two syntaxes, as `llvm::assemble`.
        // alloc: once per text a process assembles (`asm::assembled_object`)
        Arch::X86_64 => ("x86-64", format!(".intel_syntax noprefix\n{text}\n.att_syntax\n")),
        // alloc: as above
        Arch::Aarch64 => ("aarch64", format!("{text}\n")),
        Arch::Wasm32 | Arch::Wasm64 => {
            // alloc: the error's message
            return Err(format!("inline assembly for {}: wasm has no assembly", target.triple()));
        }
    };
    type Writer = fn(&Assembler) -> Result<Vec<u8>, output::OutputError>;
    let (format, write): (Format, Writer) = match target.object_format {
        ObjectFormat::Elf => (Format::Elf, output::elf::build),
        ObjectFormat::MachO => (Format::MachO, output::macho::build),
        ObjectFormat::Coff => (Format::Coff, output::coff::build),
        ObjectFormat::Wasm => {
            // alloc: the error's message
            return Err(format!(
                "inline assembly for {}: wasm objects have no assembly",
                target.triple()
            ));
        }
    };
    let arch = arch::lookup(cpu)
        // alloc: the error's message
        .ok_or_else(|| {
            format!("inline assembly for {}: rsasm is built without `{cpu}`", target.triple())
        })?;
    let dialect = arch.default_dialect();
    // A Mach-O object's deployment target (`LC_BUILD_VERSION`) comes from an LLVM triple with a
    // version (`Options::with_target_triple("arm64-apple-macosx11.0.0")`), which `Target` does
    // not carry yet, so a Mach-O object from here has none: the seed's targets are ELF.
    let options = Options::new().with_format(format).with_dialect(dialect);
    let mut asm = Assembler::new(arch, options);
    asm.assemble_str(NAME, &text);
    let finished = asm.finish();
    if !finished || asm.diags().has_errors() {
        // alloc: the error's message
        return Err(format!("inline assembly: {}", said(&asm)));
    }
    // alloc: the error's message
    write(&asm).map_err(|e| format!("inline assembly: rsasm wrote no object: {e}"))
}

/// Each error rsasm gave, as LLVM's assembler words its: `<inline asm>:L:C: error: what`,
/// joined by `; `. An error at no place is said without one.
fn said(asm: &Assembler) -> String {
    // alloc: the error's message
    let mut out = String::new();
    for diagnostic in asm.diags().iter().filter(|it| it.severity == Severity::Error) {
        if !out.is_empty() {
            out.push_str("; ");
        }
        let at = (!diagnostic.span.is_dummy())
            .then(|| {
                asm.source_map()
                    .lookup(diagnostic.span.lo)
                    .map(|file| file.line_col(diagnostic.span.lo))
            })
            .flatten();
        _ = match at {
            Some(at) => write!(out, "{NAME}:{}:{}: error: {}", at.line, at.col, diagnostic.msg),
            None => write!(out, "error: {}", diagnostic.msg),
        };
    }
    if out.is_empty() {
        out.push_str("rsasm stopped and named no error");
    }
    out
}
