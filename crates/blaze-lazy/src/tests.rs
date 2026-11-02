#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod abi;
#[cfg(not(blaze_check))]
mod budget;
mod compile;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod exec;
mod flight;
mod hit;
mod plan;
mod refuse;
mod sim;
mod stubs;
mod wake;
mod wire;

use std::collections::BTreeMap;

use blaze_codegen::{
    Arch, CompiledFunction, InstanceKey, RelocKind, Target,
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Toolchain},
};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use crate::plan::Function;

pub(crate) fn policy(level: OptLevel, mode: Mode) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            arch: Arch::X86_64,
            cpu: "x86-64-v3".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: Some("23.1".to_owned()),
            stencils: None,
            frame_pointers: true,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: level,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
    }
}

/// The simulations' and property tests' randomness, the same for a seed on every machine.
pub(crate) use blaze_ids::rng::SplitMix64 as Rng;

pub(crate) fn id(domain: &'static str, n: u32) -> ContentId {
    ContentId::of(domain, &n.to_le_bytes())
}

pub(crate) fn key(n: u32) -> InstanceKey {
    InstanceKey(id("blaze-lazy test instance", n))
}

pub(crate) fn function(n: u32) -> Function {
    Function { key: key(n), code: CodeHash(id("blaze-lazy test code", n)), item: ItemKey(id("blaze-lazy test item", n)) }
}

/// A compiled x86-64 function calling each of `callees` (`call rel32`) and taking the
/// address of each of `addressed` (`lea rel32`).
pub(crate) fn compiled(n: u32, callees: &[u32], addressed: &[u32]) -> CompiledFunction {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for &callee in callees {
        code.push(0xe8);
        relocs.push(blaze_codegen::Relocation { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(callee)), addend: -4 });
        code.extend_from_slice(&[0; 4]);
    }
    for &other in addressed {
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        relocs.push(blaze_codegen::Relocation { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(other)), addend: -4 });
        code.extend_from_slice(&[0; 4]);
    }
    code.push(0xc3);
    CompiledFunction {
        key: key(n),
        arch: Arch::X86_64,
        align: 16,
        code,
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}
