//! `.ARM.attributes`: what the object says about the CPU it was assembled
//! for, and the directives that change it.
//!
//! GNU as writes the section into every ARM object, and `objdump` reads it
//! to know which instruction set to disassemble against: without it an
//! `e12fff1e` prints as `msr SP_hyp, lr, lsl pc` rather than `bx lr`. The
//! tags are the EABI's, and the values are GNU as's for the CPU and the
//! floating-point unit selected — rsasm's ARM backend is `armv7ve` with
//! `neon-vfpv4`, which is what `tools/xas-diff` runs the reference as, so a
//! file with none of these directives in it gets what GNU as writes for that
//! command line: `Tag_CPU_name` `7VE`, `Tag_CPU_arch` 10 (ARMv7) with
//! profile `A`, the ARM and Thumb ISA tags, `Tag_FP_arch` 5 (VFPv4) with
//! `Tag_Advanced_SIMD_arch` 2, and the multiprocessing, divide and
//! virtualization tags ARMv7VE brings.
//!
//! # What is selected
//!
//! `.arch`, `.cpu`, `.fpu`, `.arch_extension` and `.object_arch` each change
//! one part of the selection, which is kept in [`ArchState::features`]: an
//! index into [`attr_data::ARCHS`] or [`attr_data::CPUS`], one into
//! [`attr_data::FPUS`], the extensions added since the last `.arch` as a
//! bitmask over that CPU's list, and the `.object_arch` override. Zero
//! everywhere is the backend's own CPU and unit.
//!
//! `.arch` and `.cpu` read tables that overlap without agreeing — `.arch
//! xscale` writes `Tag_CPU_name` `xscale` and `.cpu xscale` writes `XSCALE`
//! — and each forgets the extensions added to the CPU before it. `.fpu`
//! *replaces* every tag a unit decides, which is not what GNU as's `-mfpu=`
//! option does (that merges the unit with the CPU's own, so `-mcpu=cortex-x1
//! -mfpu=neon` keeps the ARMv8 unit where `.cpu cortex-x1` with `.fpu neon`
//! does not). `.object_arch` replaces `Tag_CPU_arch` and
//! `Tag_CPU_arch_profile` alone, and wins over an `.arch` either side of it.
//! `.eabi_attribute` replaces one tag outright, and is applied by the core
//! once the object is laid out; see [`Request::Attribute`].
//!
//! [`ArchState::features`]: crate::arch::ArchState::features
//! [`Request::Attribute`]: crate::arch::Request::Attribute
//!
//! # How the tags are put together
//!
//! `src/arch/arm/attr_data.rs` is measured from the reference by
//! `tools/tables/arm-attrs.py`, through the directives and under that
//! command line. A CPU with one unit, and a CPU with one extension, are
//! therefore exactly what GNU as writes, and the generator assembles every
//! such pair to prove it.
//!
//! Beyond that the answer is put together rather than measured, since GNU as
//! merges feature *bits* and derives the tags from the result, which is not
//! the same operation. Where two choices give one tag two values the larger
//! wins, except for `Tag_FP_arch`, whose `-d16` halves are numbered after the
//! units they are half of, and `Tag_Virtualization_use`, which is a pair of
//! bits. So a second `.arch_extension`, or one beside an `.fpu`, can differ
//! from GNU as; either directive on its own does not.
//!
//! `.arch_extension no<name>` is taken and changes nothing, which is what
//! GNU as 2.47 does with it: `-march=armv7ve` keeps `Tag_DIV_use` through
//! `.arch_extension noidiv`, and an `mp` the directive added survives
//! `.arch_extension nomp`.
//!
//! # Which instructions the selection has
//!
//! That is a second model, in [`super::cpu`], over the same names: these
//! tags are measured through GNU as, and what a target has is read out of
//! binutils, so the two can disagree about a tag without disagreeing about
//! an instruction. `.arch armv4t` refuses a `clz` there and writes
//! `Tag_CPU_arch` 2 here.

use super::attr_data::{ARCHS, CPUS, FP_TAGS, FPUS};
use crate::arch::{ArchState, AttrSection, AttrValue};

/// A floating-point unit, or an extension as one CPU sees it.
pub(crate) struct Named {
    pub key: &'static str,
    /// The tags it decides, by tag number.
    pub tags: &'static [(u8, u8)],
}

/// One `.arch` or `.cpu` name and the tags GNU as writes for it.
pub(crate) struct Cpu {
    pub key: &'static str,
    /// `Tag_CPU_name`, which is not always the spelling of `key`.
    pub cpu_name: &'static str,
    /// Every other tag the directive leaves behind, in tag order.
    pub tags: &'static [(u8, u8)],
    /// The `.arch_extension` names this CPU takes, and what each changes.
    pub exts: &'static [Named],
}

// How the selection is packed into the state's `features` word, which is
// zero for the backend's own CPU and unit. Each index is one more than the
// entry's, so that zero can mean "none chosen".
const CPU_SHIFT: u32 = 0;
const CPU_BITS: u64 = 0x1ff;
const FPU_SHIFT: u32 = 9;
const FPU_BITS: u64 = 0x3f;
const OBJ_SHIFT: u32 = 15;
const OBJ_BITS: u64 = 0x1ff;
/// The extensions added to the selected CPU, as a bitmask over its own list.
const EXT_SHIFT: u32 = 24;

/// The `-march=` rsasm's ARM backend is, and what `tools/xas-diff` runs the
/// reference as.
const DEFAULT_ARCH: &str = "armv7ve";

/// Tag_CPU_name, the one tag of this set whose value is a string.
const TAG_CPU_NAME: u32 = 5;
/// Tag_CPU_arch and Tag_CPU_arch_profile, the two `.object_arch` replaces.
const TAG_CPU_ARCH: u8 = 6;
const TAG_CPU_ARCH_PROFILE: u8 = 7;
/// Tag_FP_arch, numbered so that a `-d16` unit follows the one it is half of.
const TAG_FP_ARCH: u8 = 10;
/// Tag_Virtualization_use: bit 0 is TrustZone and bit 1 the virtualization
/// extensions, so two extensions that each set one make 3.
const TAG_VIRTUALIZATION_USE: u8 = 68;

/// `Tag_FP_arch` in order of capability: 3 (VFPv3) is more than 4
/// (VFPv3-d16), and so on up.
const FP_ARCH_ORDER: [u8; 9] = [0, 1, 2, 4, 3, 6, 5, 8, 7];

fn fp_arch_rank(v: u8) -> usize {
    FP_ARCH_ORDER
        .iter()
        .position(|&x| x == v)
        .unwrap_or(v as usize)
}

/// What one tag becomes where the unit and an extension, or two extensions,
/// each give it a value.
fn combine(tag: u8, old: u8, new: u8) -> u8 {
    match tag {
        TAG_FP_ARCH if fp_arch_rank(new) > fp_arch_rank(old) => new,
        TAG_FP_ARCH => old,
        TAG_VIRTUALIZATION_USE => old | new,
        _ => old.max(new),
    }
}

/// The CPU the state has selected.
fn selected(state: &ArchState) -> &'static Cpu {
    let idx = ((state.features >> CPU_SHIFT) & CPU_BITS) as usize;
    match idx.checked_sub(1) {
        None => ARCHS
            .iter()
            .find(|c| c.key == DEFAULT_ARCH)
            .expect("the default architecture is in the table"),
        Some(i) if i < ARCHS.len() => &ARCHS[i],
        Some(i) => &CPUS[i - ARCHS.len()],
    }
}

/// `.arch`, which selects an architecture and forgets the extensions added
/// to the one before it.
pub(crate) fn set_arch(state: &mut ArchState, name: &str) -> bool {
    match ARCHS.iter().position(|c| c.key == name) {
        Some(i) => {
            set_selected(state, i + 1);
            true
        }
        None => false,
    }
}

/// `.cpu`, which names a CPU rather than an architecture.
pub(crate) fn set_cpu(state: &mut ArchState, name: &str) -> bool {
    match CPUS.iter().position(|c| c.key == name) {
        Some(i) => {
            set_selected(state, ARCHS.len() + i + 1);
            true
        }
        None => false,
    }
}

fn set_selected(state: &mut ArchState, idx: usize) {
    let keep = state.features & !((CPU_BITS << CPU_SHIFT) | (u64::MAX << EXT_SHIFT));
    state.features = keep | ((idx as u64 & CPU_BITS) << CPU_SHIFT);
}

/// `.fpu`.
pub(crate) fn set_fpu(state: &mut ArchState, name: &str) -> bool {
    match FPUS.iter().position(|f| f.key == name) {
        Some(i) => {
            state.features &= !(FPU_BITS << FPU_SHIFT);
            state.features |= (i as u64 + 1) << FPU_SHIFT;
            true
        }
        None => false,
    }
}

/// `.arch_extension`, and its `no` form, which GNU as 2.47 takes without
/// changing an attribute.
pub(crate) fn set_extension(state: &mut ArchState, name: &str) -> bool {
    let exts = selected(state).exts;
    let (name, add) = match name.strip_prefix("no") {
        Some(rest) if exts.iter().any(|e| e.key == rest) => (rest, false),
        _ => (name, true),
    };
    let Some(bit) = exts.iter().position(|e| e.key == name) else {
        return false;
    };
    if add {
        state.features |= 1 << (EXT_SHIFT + bit as u32);
    }
    true
}

/// `.object_arch`, which says what the object may be linked as: only
/// `Tag_CPU_arch` and `Tag_CPU_arch_profile` follow it, and it wins over an
/// `.arch` either side of it.
pub(crate) fn set_object_arch(state: &mut ArchState, name: &str) -> bool {
    match ARCHS.iter().position(|c| c.key == name) {
        Some(i) => {
            state.features &= !(OBJ_BITS << OBJ_SHIFT);
            state.features |= (i as u64 + 1) << OBJ_SHIFT;
            true
        }
        None => false,
    }
}

/// The `aeabi` tags the object gets, in increasing tag order.
fn tags(state: &ArchState) -> Vec<(u32, AttrValue)> {
    let cpu = selected(state);
    let mut out: Vec<(u8, u8)> = cpu.tags.to_vec();
    let fpu_idx = ((state.features >> FPU_SHIFT) & FPU_BITS) as usize;
    let fpu = fpu_idx.checked_sub(1).map(|i| &FPUS[i]);
    if let Some(fpu) = fpu {
        out.retain(|&(t, _)| !FP_TAGS.contains(&t));
        out.extend_from_slice(fpu.tags);
    }
    let mask = state.features >> EXT_SHIFT;
    for (bit, ext) in cpu.exts.iter().enumerate() {
        if mask & (1 << bit) == 0 {
            continue;
        }
        for &(t, v) in ext.tags {
            // An extension replaces what the CPU said, and combines with
            // what the unit or another extension said; see the module note.
            let shared = fpu.is_some_and(|f| f.tags.iter().any(|&(x, _)| x == t))
                || applied_elsewhere(cpu, mask, bit, t);
            match out.iter_mut().find(|(x, _)| *x == t) {
                Some(slot) if shared => slot.1 = combine(t, slot.1, v),
                Some(slot) => slot.1 = v,
                None => out.push((t, v)),
            }
        }
    }
    if let Some(i) = (((state.features >> OBJ_SHIFT) & OBJ_BITS) as usize).checked_sub(1) {
        let object = &ARCHS[i];
        for tag in [TAG_CPU_ARCH, TAG_CPU_ARCH_PROFILE] {
            out.retain(|&(t, _)| t != tag);
            if let Some(&(_, v)) = object.tags.iter().find(|&&(t, _)| t == tag) {
                out.push((tag, v));
            }
        }
    }
    out.sort_by_key(|&(t, _)| t);
    let mut tags: Vec<(u32, AttrValue)> = out
        .into_iter()
        .map(|(t, v)| (u32::from(t), AttrValue::Int(u64::from(v))))
        .collect();
    if !cpu.cpu_name.is_empty() {
        tags.insert(0, (TAG_CPU_NAME, AttrValue::Str(cpu.cpu_name.to_string())));
    }
    tags
}

/// Whether another extension the state has added also decides tag `t`.
fn applied_elsewhere(cpu: &'static Cpu, mask: u64, bit: usize, t: u8) -> bool {
    cpu.exts
        .iter()
        .enumerate()
        .any(|(b, e)| b != bit && mask & (1 << b) != 0 && e.tags.iter().any(|&(x, _)| x == t))
}

/// The section itself.
pub(crate) fn section(state: &ArchState) -> Vec<AttrSection> {
    vec![AttrSection::attributes(
        ".ARM.attributes",
        "aeabi",
        tags(state),
    )]
}
