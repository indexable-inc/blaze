//! Which SH CPUs an instruction runs on, in GNU as's terms, and the `e_flags`
//! that follow from it.
//!
//! GNU as tags every opcode with an *architecture set* (`opcodes/sh-opc.h`):
//! one bit per branch of the SH family tree, one per MMU choice and one per
//! coprocessor choice. A set names every CPU that has the instruction, so
//! `movca.l` is tagged "SH-4 without MMU or FPU, and everything above it".
//! As it assembles, GNU as intersects the sets of the opcodes it picks
//! (`valid_arch` in `gas/config/tc-sh.c`), and at the end writes the ELF
//! machine of the least capable CPU left in the intersection
//! (`sh_get_bfd_mach_from_arch_set` in `bfd/cpu-sh.c`). Data, labels and
//! alignment play no part, so a file with no instructions is marked SH-1.
//!
//! The constants here are those sets, bit for bit, so the table in
//! [`super::insn`] can carry GNU's tag for each entry unchanged. A set is
//! *valid* when it still has a bit in each of the three groups; an instruction
//! whose set would leave the running intersection invalid is rejected, which
//! is how a CPU name such as `sh2` refuses what that CPU lacks. Such a name
//! starts the intersection from that one CPU, as `sh-elf-as --isa=sh2` does,
//! so its objects carry that CPU's flags whatever they contain.

use crate::arch::ArchState;

// ---- the bits ---------------------------------------------------------------

const SH1_BASE: u32 = 1 << 0;
const SH2_BASE: u32 = 1 << 1;
const SH2A_SH3_BASE: u32 = 1 << 2;
const SH3_BASE: u32 = 1 << 3;
const SH2A_SH4_BASE: u32 = 1 << 4;
const SH4_BASE: u32 = 1 << 5;
const SH4A_BASE: u32 = 1 << 6;
const SH2A_BASE: u32 = 1 << 7;
const BASE_MASK: u32 = 0xff;

const NO_MMU: u32 = 1 << 26;
const HAS_MMU: u32 = 1 << 27;
const MMU_MASK: u32 = NO_MMU | HAS_MMU;

/// Neither an FPU nor a DSP.
const NO_CO: u32 = 1 << 28;
const SP_FPU: u32 = 1 << 29;
const DP_FPU: u32 = 1 << 30;
const HAS_DSP: u32 = 1 << 31;
const CO_MASK: u32 = NO_CO | SP_FPU | DP_FPU | HAS_DSP;

// ---- single CPUs ------------------------------------------------------------
//
// The two-name sets are GNU's own: a CPU that is "either of these", used for
// instructions the two share and nothing below them has.

pub const SH1: u32 = SH1_BASE | NO_MMU | NO_CO;
pub const SH2: u32 = SH2_BASE | NO_MMU | NO_CO;
const SH2A: u32 = SH2A_BASE | NO_MMU | DP_FPU;
const SH2A_NOFPU: u32 = SH2A_BASE | NO_MMU | NO_CO;
pub const SH2E: u32 = SH2_BASE | NO_MMU | SP_FPU;
const SH_DSP: u32 = SH2_BASE | NO_MMU | HAS_DSP;
const SH3_NOMMU: u32 = SH3_BASE | NO_MMU | NO_CO;
pub const SH3: u32 = SH3_BASE | HAS_MMU | NO_CO;
pub const SH3E: u32 = SH3_BASE | HAS_MMU | SP_FPU;
const SH3_DSP: u32 = SH3_BASE | HAS_MMU | HAS_DSP;
pub const SH4: u32 = SH4_BASE | HAS_MMU | DP_FPU;
pub const SH4A: u32 = SH4A_BASE | HAS_MMU | DP_FPU;
const SH4AL_DSP: u32 = SH4A_BASE | HAS_MMU | HAS_DSP;
const SH4_NOFPU: u32 = SH4_BASE | HAS_MMU | NO_CO;
const SH4A_NOFPU: u32 = SH4A_BASE | HAS_MMU | NO_CO;
const SH4_NOMMU_NOFPU: u32 = SH4_BASE | NO_MMU | NO_CO;
const SH2A_NOFPU_OR_SH4_NOMMU_NOFPU: u32 = SH2A_SH4_BASE | NO_MMU | NO_CO;
const SH2A_NOFPU_OR_SH3_NOMMU: u32 = SH2A_SH3_BASE | NO_MMU | NO_CO;
const SH2A_OR_SH3E: u32 = SH2A_SH4_BASE | NO_MMU | SP_FPU;
const SH2A_OR_SH4: u32 = SH2A_SH4_BASE | NO_MMU | DP_FPU;

// ---- a CPU and everything above it ------------------------------------------

pub const SH_UP: u32 = SH1 | SH2_UP;
pub const SH2_UP: u32 = SH2 | SH2E_UP | SH2A_NOFPU_OR_SH3_NOMMU_UP | SH_DSP_UP;
pub const SH2A_NOFPU_OR_SH3_NOMMU_UP: u32 =
    SH2A_NOFPU_OR_SH3_NOMMU | SH2A_NOFPU_OR_SH4_NOMMU_NOFPU_UP | SH2A_OR_SH3E_UP | SH3_NOMMU_UP;
const SH2A_NOFPU_OR_SH4_NOMMU_NOFPU_UP: u32 =
    SH2A_NOFPU_OR_SH4_NOMMU_NOFPU | SH2A_NOFPU_UP | SH2A_OR_SH4_UP | SH4_NOMMU_NOFPU_UP;
const SH2A_NOFPU_UP: u32 = SH2A_NOFPU | SH2A_UP;
pub const SH3_NOMMU_UP: u32 = SH3_NOMMU | SH3_UP | SH4_NOMMU_NOFPU_UP;
pub const SH3_UP: u32 = SH3 | SH3E_UP | SH3_DSP_UP | SH4_NOFPU_UP;
pub const SH4_NOMMU_NOFPU_UP: u32 = SH4_NOMMU_NOFPU | SH4_NOFPU_UP;
const SH4_NOFPU_UP: u32 = SH4_NOFPU | SH4_UP | SH4A_NOFPU_UP;
pub const SH4A_NOFPU_UP: u32 = SH4A_NOFPU | SH4A_UP | SH4AL_DSP_UP;
pub const SH2E_UP: u32 = SH2E | SH2A_OR_SH3E_UP;
pub const SH2A_OR_SH3E_UP: u32 = SH2A_OR_SH3E | SH2A_OR_SH4_UP | SH3E_UP;
pub const SH2A_OR_SH4_UP: u32 = SH2A_OR_SH4 | SH2A_UP | SH4_UP;
const SH2A_UP: u32 = SH2A;
const SH3E_UP: u32 = SH3E | SH4_UP;
pub const SH4_UP: u32 = SH4 | SH4A_UP;
pub const SH4A_UP: u32 = SH4A;
const SH_DSP_UP: u32 = SH_DSP | SH3_DSP_UP;
const SH3_DSP_UP: u32 = SH3_DSP | SH4AL_DSP_UP;
const SH4AL_DSP_UP: u32 = SH4AL_DSP;

/// What GNU as starts from when not given `--isa`: every CPU but the DSPs.
pub const DEFAULT: u32 = SH_UP & !HAS_DSP;

/// Whether `set` still describes some CPU.
pub const fn valid(set: u32) -> bool {
    set & BASE_MASK != 0 && set & MMU_MASK != 0 && set & CO_MASK != 0
}

/// GNU as's `valid_arch` at this point in the source.
///
/// [`ArchState::features`] holds the set the target starts from, [`DEFAULT`]
/// or one CPU, and [`ArchState::used`] the bits the forms assembled since
/// have ruled out. Keeping the complement means `used` starts at zero, like
/// every other backend's, and an `.arch` switch that resets the state starts
/// the intersection over.
pub fn remaining(state: &ArchState) -> u32 {
    state.features as u32 & !(state.used as u32)
}

/// Narrows the running intersection to the CPUs in `set`.
pub fn record(state: &mut ArchState, set: u32) {
    state.used |= u64::from(!set);
}

/// Names the CPUs an instruction tagged `set` needs, for a diagnostic.
pub fn describe(set: u32) -> &'static str {
    match set {
        SH2_UP => "SH-2",
        SH2A_NOFPU_OR_SH3_NOMMU_UP | SH3_NOMMU_UP | SH3_UP => "SH-3",
        SH4_NOMMU_NOFPU_UP => "SH-4",
        SH4A_NOFPU_UP | SH4A_UP => "SH-4A",
        SH2E_UP => "an SH-2E, SH-3E or SH-4 FPU",
        SH2A_OR_SH3E_UP => "an SH-3E or SH-4 FPU",
        SH2A_OR_SH4_UP | SH4_UP => "the SH-4 double-precision FPU",
        _ => "a different SH variant",
    }
}

/// `bfd_to_arch_table` from `bfd/cpu-sh.c`, in its order, with each machine
/// written as the `EF_SH_*` value `bfd/elf32-sh.c` maps it to.
const MACHINES: &[(u32, u32)] = &[
    (1, SH_UP),                             // EF_SH1
    (2, SH2_UP),                            // EF_SH2
    (11, SH2E_UP),                          // EF_SH2E
    (4, SH_DSP_UP),                         // EF_SH_DSP
    (13, SH2A_UP),                          // EF_SH2A
    (19, SH2A_NOFPU_UP),                    // EF_SH2A_NOFPU
    (21, SH2A_NOFPU_OR_SH4_NOMMU_NOFPU_UP), // EF_SH2A_SH4_NOFPU
    (22, SH2A_NOFPU_OR_SH3_NOMMU_UP),       // EF_SH2A_SH3_NOFPU
    (23, SH2A_OR_SH4_UP),                   // EF_SH2A_SH4
    (24, SH2A_OR_SH3E_UP),                  // EF_SH2A_SH3E
    (3, SH3_UP),                            // EF_SH3
    (20, SH3_NOMMU_UP),                     // EF_SH3_NOMMU
    (5, SH3_DSP_UP),                        // EF_SH3_DSP
    (8, SH3E_UP),                           // EF_SH3E
    (9, SH4_UP),                            // EF_SH4
    (12, SH4A_UP),                          // EF_SH4A
    (6, SH4AL_DSP_UP),                      // EF_SH4AL_DSP
    (16, SH4_NOFPU_UP),                     // EF_SH4_NOFPU
    (18, SH4_NOMMU_NOFPU_UP),               // EF_SH4_NOMMU_NOFPU
    (17, SH4A_NOFPU_UP),                    // EF_SH4A_NOFPU
];

/// The `e_flags` GNU as writes for a file whose instructions left `set`.
///
/// This is `sh_get_bfd_mach_from_arch_set`. Conceptually it picks the
/// machine whose "and above" set adds the fewest CPUs outside `set`, and of
/// those the one that leaves out the fewest inside it; in practice it
/// compares the leftover bits as plain numbers, as here, and an earlier row
/// wins a tie. When `set` allows a CPU with no coprocessor, the coprocessor
/// bits are masked out of every row first, so the FPU and DSP bits an
/// integer-only file excludes do not make an FPU machine the closer fit.
pub fn elf_flags(set: u32) -> u32 {
    let co_mask = if set & NO_CO != 0 {
        !(SP_FPU | DP_FPU | HAS_DSP)
    } else {
        !0
    };
    // GNU as finds no machine only for an invalid set, which the gating in
    // `encode` never lets happen; it would then write 15, the last `EF_SH_*`
    // whose machine is 0.
    let mut result = 15;
    let mut best = !set;
    for &(flags, up) in MACHINES {
        let try_ = up & co_mask;
        let extra = try_ & !set;
        let best_extra = best & !set;
        if (extra < best_extra || (extra == best_extra && (!try_ & set) < (!best & set)))
            && valid(try_ & set)
        {
            result = flags;
            best = try_;
        }
    }
    result
}
