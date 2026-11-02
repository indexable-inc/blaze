//! The stencil generator, run by `build.rs` (stencil.md, "Why a build script"). For each
//! [`Variant`], on any host: the IR of every shape of its set as one module ([`ir`], with the
//! op bodies of [`ops`]), compiled by LLVM to one ELF object ([`llvm`]), read back into
//! stencils with every check ([`elf`]), and written as the variant's library to `OUT_DIR`.
//! The three variants are made on three threads; each has its own LLVM context.

mod elf;
mod ir;
mod llvm;
mod ops;

use std::{env, path::PathBuf};

use crate::{
    format::{Variant, Writer},
    shape::{self, Shape},
};

const PREFIX: &str = "LLVM_SYS_231_PREFIX";

pub fn main() {
    for path in ["build.rs", "gen", "src/shape.rs", "src/enc.rs", "src/table/format.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed={PREFIX}");
    let prefix = env::var_os(PREFIX).filter(|it| !it.is_empty()).unwrap_or_else(|| {
        panic!(
            "blaze-stencil's stencils are made by the pinned LLVM, and {PREFIX} is unset: enter \
             `nix develop`, or fetch it with `python3 .github/scripts/llvm.py fetch --target \
             <triple> --dir <dir>` and set {PREFIX} to that directory"
        )
    });
    println!("cargo:rerun-if-changed={}", PathBuf::from(prefix).join("blaze-llvm.json").display());
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));

    llvm::init();
    let mut refused = Vec::new();
    std::thread::scope(|scope| {
        let threads = Variant::ALL.map(|variant| scope.spawn(move || (variant, library(variant))));
        for thread in threads {
            let (variant, library) = match thread.join() {
                Ok(it) => it,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            match library {
                Ok(bytes) => {
                    let path = out.join(variant.file());
                    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                }
                Err(refusals) => refused.extend(refusals),
            }
        }
    });
    if !refused.is_empty() {
        panic!(
            "{} stencil instructions are refused (a stencil is leaf and stackless):\n{}",
            refused.len(),
            refused.join("\n")
        );
    }
}

/// The library of `variant`'s stencils, or every refused instruction in them.
fn library(variant: Variant) -> Result<Vec<u8>, Vec<String>> {
    let isa = variant.isa();
    let mut shapes: Vec<(u32, Shape)> = Vec::with_capacity(8192);
    shape::each_on(isa, |shape| {
        if variant.has(shape) {
            shapes.push((shapes.len() as u32, shape));
        }
    });
    let machine = llvm::Machine::of(variant);
    let name = format!("{variant:?}");
    let text = ir::module(isa, &shapes);
    let object = llvm::compile(machine, &name, &text).unwrap_or_else(|e| panic!("{e}"));
    let disasm = llvm::Disasm::new(machine).unwrap_or_else(|e| panic!("{e}"));
    let elf = elf::Elf::new(&object, isa, &name);
    let mut writer = Writer::new(variant);
    elf.probe(ir::PROBE, &name, &disasm, &mut writer);
    for (index, shape) in &shapes {
        let what = format!("{shape} ({name})");
        let stencil = elf.stencil(&ir::name(*index), &what, Some(*shape), &disasm, &mut writer);
        let at = writer
            .push_with_faults(*shape, &stencil.code, stencil.tail, &stencil.holes, &stencil.faults)
            .unwrap_or_else(|e| panic!("stencil `{what}`: {e}"));
        assert_eq!(at, *index, "stencil `{what}` was written out of the set's order");
    }
    let refusals = elf.refusals();
    if !refusals.is_empty() {
        return Err(refusals);
    }
    Ok(writer.finish(blaze_ids::b3::hash).unwrap_or_else(|e| panic!("the {name} library: {e}")))
}
