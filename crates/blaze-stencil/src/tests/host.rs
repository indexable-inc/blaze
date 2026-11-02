//! The targets the tests compile for, each with the Cranelift ISA `hir-ty-codegen/target.rs`
//! makes for it (the same flags: `is_pic`, `tls_model`, inline probes, frame pointers,
//! `opt_level=none`, LLVM's ABI extensions, `has_cmpxchg16b`, `has_lse` on `apple-m1`), so
//! Cranelift's code of a function here is the code the dev path's Cranelift backend makes.

use cranelift_codegen::{
    Context,
    control::ControlPlane,
    ir,
    isa::{self, CallConv, OwnedTargetIsa},
    settings::{self, Configurable},
};

use crate::{Cpu, Os, Target, TlsModel, code};

/// The targets the IR builder makes code for (`hir-ty-codegen/target.rs`): ELF x86-64 and
/// AArch64, Mach-O AArch64 (`apple-m1`), COFF x86-64.
pub const TARGETS: [Target; 4] = [
    Target {
        arch: blaze_codegen::Arch::X86_64,
        os: Os::Linux,
        cpu: Cpu::Baseline,
        pic: true,
        tls: TlsModel::ElfGd,
        probe_stack: true,
        probe_log2: 12,
    },
    Target {
        arch: blaze_codegen::Arch::Aarch64,
        os: Os::Linux,
        cpu: Cpu::Baseline,
        pic: true,
        tls: TlsModel::ElfGd,
        probe_stack: true,
        probe_log2: 12,
    },
    Target {
        arch: blaze_codegen::Arch::Aarch64,
        os: Os::MacOs,
        cpu: Cpu::AppleM1,
        pic: true,
        tls: TlsModel::MachO,
        probe_stack: true,
        probe_log2: 12,
    },
    Target {
        arch: blaze_codegen::Arch::X86_64,
        os: Os::Windows,
        cpu: Cpu::Baseline,
        pic: false,
        tls: TlsModel::Coff,
        probe_stack: true,
        probe_log2: 12,
    },
];

/// The target of the machine the tests run on, where they can run code.
pub fn host() -> Option<Target> {
    TARGETS.into_iter().find(|target| {
        let arch = match target.arch {
            blaze_codegen::Arch::X86_64 => cfg!(target_arch = "x86_64"),
            blaze_codegen::Arch::Aarch64 => cfg!(target_arch = "aarch64"),
            blaze_codegen::Arch::Wasm32 => false,
        };
        let os = match target.os {
            Os::Linux => cfg!(target_os = "linux"),
            Os::MacOs => cfg!(target_os = "macos"),
            Os::Windows => cfg!(target_os = "windows"),
        };
        arch && os
    })
}

pub fn triple(target: &Target) -> &'static str {
    match (target.arch, target.os) {
        (blaze_codegen::Arch::X86_64, Os::Linux) => "x86_64-unknown-linux-gnu",
        (blaze_codegen::Arch::X86_64, Os::MacOs) => "x86_64-apple-darwin",
        (blaze_codegen::Arch::X86_64, Os::Windows) => "x86_64-pc-windows-msvc",
        (blaze_codegen::Arch::Aarch64, Os::Linux) => "aarch64-unknown-linux-gnu",
        (blaze_codegen::Arch::Aarch64, Os::MacOs) => "aarch64-apple-darwin",
        (blaze_codegen::Arch::Aarch64, Os::Windows) => "aarch64-pc-windows-msvc",
        (blaze_codegen::Arch::Wasm32, _) => "wasm32-unknown-unknown",
    }
}

/// The convention the IR builder gives functions of `target`.
pub fn call_conv(target: &Target) -> CallConv {
    match (target.arch, target.os) {
        (blaze_codegen::Arch::Aarch64, Os::MacOs) => CallConv::AppleAarch64,
        (blaze_codegen::Arch::X86_64, Os::Windows) => CallConv::WindowsFastcall,
        _ => CallConv::SystemV,
    }
}

/// Cranelift's ISA for `target`, as `hir-ty-codegen/target.rs`'s `Target::isa` makes it with
/// frame pointers kept (the stencil backend keeps one in every function).
pub fn isa(target: &Target) -> OwnedTargetIsa {
    let mut flags = settings::builder();
    let tls = match target.tls {
        TlsModel::ElfGd => "elf_gd",
        TlsModel::MachO => "macho",
        TlsModel::Coff => "coff",
    };
    for (name, value) in [
        ("is_pic", if target.pic { "true" } else { "false" }),
        ("tls_model", tls),
        ("enable_probestack", if target.probe_stack { "true" } else { "false" }),
        ("probestack_strategy", "inline"),
        ("preserve_frame_pointers", "true"),
        ("opt_level", "none"),
        ("enable_verifier", "true"),
        ("enable_llvm_abi_extensions", "true"),
    ] {
        flags.set(name, value).unwrap_or_else(|e| panic!("cranelift setting {name}={value}: {e}"));
    }
    flags
        .set("probestack_size_log2", &target.probe_log2.to_string())
        .unwrap_or_else(|e| panic!("cranelift setting probestack_size_log2: {e}"));
    let mut builder = isa::lookup_by_name(triple(target))
        .unwrap_or_else(|e| panic!("no cranelift ISA for {}: {e}", triple(target)));
    if target.arch == blaze_codegen::Arch::X86_64 {
        builder.enable("has_cmpxchg16b").expect("has_cmpxchg16b");
    }
    if target.cpu == Cpu::AppleM1 {
        builder.enable("has_lse").expect("has_lse");
    }
    builder.finish(settings::Flags::new(flags)).expect("a cranelift ISA")
}

/// What Cranelift's compile of a function gives the tests, in the terms of the stencil
/// backend's [`code::Code`] where the two have the same thing, so [`super::exec`] links and
/// runs both the same way.
pub struct Compiled {
    pub bytes: Vec<u8>,
    pub align: u32,
    /// By offset, with the function's own names resolved as [`code::Name`]s.
    pub relocs: Vec<code::Reloc>,
    /// `(offset, code)` of each trap instruction.
    pub traps: Vec<(u32, ir::TrapCode)>,
    pub calls: Vec<code::CallSite>,
    pub handlers: Vec<code::Handler>,
    pub unwind: Option<isa::unwind::UnwindInfo>,
}

/// Cranelift's code of `func` for `isa`, or its error (a lowering it has no rule for).
pub fn cranelift(func: &ir::Function, isa: &dyn isa::TargetIsa) -> Result<Compiled, String> {
    let mut ctx = Context::for_function(func.clone());
    let compiled = match ctx.compile(isa, &mut ControlPlane::default()) {
        Ok(compiled) => compiled,
        Err(e) => return Err(format!("{:?}", e.inner)),
    };
    let bytes = compiled.code_buffer().to_vec();
    let mut relocs = Vec::with_capacity(compiled.buffer.relocs().len());
    for reloc in compiled.buffer.relocs() {
        let name = match &reloc.target {
            cranelift_codegen::RelocTarget::ExternalName(name) => name_of(func, name)?,
            cranelift_codegen::RelocTarget::Label(at) => code::Name::Own(at.as_offset()),
        };
        relocs.push(code::Reloc { at: reloc.offset, kind: reloc.kind, name, addend: reloc.addend });
    }
    let traps = compiled.buffer.traps().iter().map(|trap| (trap.offset, trap.code)).collect();
    let mut calls = Vec::new();
    let mut handlers = Vec::new();
    for site in compiled.buffer.call_sites() {
        let first = handlers.len() as u32;
        for handler in site.exception_handlers {
            match *handler {
                cranelift_codegen::MachExceptionHandler::Tag(tag, landing) => {
                    handlers.push(code::Handler::Tag { tag: tag.as_u32(), landing: landing.as_offset() });
                }
                cranelift_codegen::MachExceptionHandler::Default(landing) => {
                    handlers.push(code::Handler::Default { landing: landing.as_offset() });
                }
                // The IR builder never sets a dynamic context, and neither do the tests.
                cranelift_codegen::MachExceptionHandler::Context(_) => {
                    return Err("an exception handler with a dynamic context".to_owned());
                }
            }
        }
        calls.push(code::CallSite { ret: site.ret_addr, handlers: first..handlers.len() as u32 });
    }
    let unwind = compiled.create_unwind_info(isa).map_err(|e| format!("{e:?}"))?;
    Ok(Compiled { bytes, align: compiled.buffer.min_alignment, relocs, traps, calls, handlers, unwind })
}

/// The relocation name of `name` in `func`, as the stencil backend's glue names it
/// (`arch::Cx::name`).
fn name_of(func: &ir::Function, name: &ir::ExternalName) -> Result<code::Name, String> {
    match name {
        ir::ExternalName::User(at) => {
            let name = &func.params.user_named_funcs()[*at];
            Ok(code::Name::User { namespace: name.namespace, index: name.index })
        }
        ir::ExternalName::LibCall(call) => Ok(code::Name::LibCall(*call)),
        ir::ExternalName::KnownSymbol(symbol) => Ok(code::Name::Known(*symbol)),
        ir::ExternalName::TestCase(name) => Err(format!("a relocation against test case {name}")),
    }
}

/// [`cranelift`], with a panic in Cranelift (a lowering it asserts it has) as its error.
pub fn cranelift_caught(func: &ir::Function, isa: &dyn isa::TargetIsa) -> Result<Compiled, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cranelift(func, isa))) {
        Ok(it) => it,
        Err(panic) => Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
            .unwrap_or_else(|| "a panic".to_owned())),
    }
}

#[test]
fn every_target_has_an_isa() {
    for target in &TARGETS {
        let isa = isa(target);
        assert_eq!(isa.default_call_conv(), call_conv(target), "{}", triple(target));
    }
}
