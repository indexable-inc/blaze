//! The target architecture an `asm!` is checked for (INTRINSICS.md P9b, main's ruling (A)):
//! read as `cfg` atoms at the `asm!` itself, so the body's record holds the reads and is filed
//! under them (drive.md "One check covers every configuration": a record that read nothing
//! serves every configuration; one that read `target_arch` is that target's). The Reference,
//! inline-assembly, "Register operands": the register classes and the types each takes are
//! the architecture's, so one body's check differs between targets only through these reads.

use super::*;

/// The architectures whose `asm!` operand rules blaze tables, asked in this order; the
/// first that holds is the target's. None holding is kept as no architecture (`None`), which
/// the conversion to the checker's form refuses by name.
const ARCHES: [(&str, blaze_typeck::AsmArch); 2] =
    [("aarch64", blaze_typeck::AsmArch::Aarch64), ("x86_64", blaze_typeck::AsmArch::X86_64)];

impl Machine<'_, '_, '_> {
    /// `target_arch` for the `asm!` at `r`: each atom asked is a read of the record (its
    /// answer kept as a written atom's is, `cfg_atom`); an atom not known yet is a need at `r`
    /// and the pass's result is not kept (`Need`), so `None` then is never filed.
    pub(crate) fn asm_arch(&mut self, r: Range) -> Option<blaze_typeck::AsmArch> {
        for (value, arch) in ARCHES {
            let Some(cfg) = self.cfg.as_deref_mut() else {
                unreachable!("the cfg set is lent only while a call expands, and no step runs then")
            };
            let answer = cfg.atom("target_arch", Some(value));
            match answer {
                CfgAnswer::Need => {
                    self.needs.push(r);
                    return None;
                }
                CfgAnswer::Yes | CfgAnswer::No => {
                    let read = CfgRead {
                        name: Name::of("target_arch").0,
                        value: Name::of(value).0,
                        answer: answer == CfgAnswer::Yes,
                    };
                    self.out.cfg.push(read.encode());
                    if answer == CfgAnswer::Yes {
                        return Some(arch);
                    }
                }
            }
        }
        None
    }
}
