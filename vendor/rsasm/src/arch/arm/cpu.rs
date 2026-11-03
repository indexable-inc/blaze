//! Which instructions the target selected has, and the check every
//! instruction goes through.
//!
//! GNU as keeps one `arm_feature_set` — three 32-bit words of core features
//! and one of coprocessor ones — built out of three parts, and lets an
//! instruction through when that set and the instruction's have any bit in
//! common (`ARM_CPU_HAS_FEATURE`). Any, not all: a form an ARMv6T2 core *or*
//! an ARMv8-M one has names both bits, and either will do. This module is
//! the same three parts and the same test:
//!
//! * the architecture or CPU selected, which `.arch`, `.cpu`, `-march=` and
//!   `-mcpu=` replace outright;
//! * the extensions added since, which `.arch_extension` and the `+name`
//!   suffixes of `-march=` and `-mcpu=` add to and `no<name>` takes away
//!   from, and which selecting an architecture or CPU forgets;
//! * the floating-point and SIMD unit, which `.fpu` and `-mfpu=` replace and
//!   which otherwise is the one the architecture or CPU brings.
//!
//! A source that names none of them gets [`DEFAULT_ARCH`] with
//! [`DEFAULT_FPU`] — `armv7ve` and `neon-vfpv4`, the command line
//! `tools/xas-diff` and `tools/mc-diff` assemble the reference with, and so
//! the target every ARM corpus here was measured against. GNU as's own
//! default for `arm-none-eabi` is not that: with no `-march` it allows every
//! core instruction it knows, up to ARMv9, and with no `-mfpu` no
//! floating-point instruction at all. Neither half of that would suit this
//! backend, whose instructions are an ARMv7VE core's with a VFPv4 and NEON
//! unit: a `crc32b` is an unknown instruction here either way, and refusing
//! `vadd.f32` until a `.fpu` said so would refuse what these corpora
//! assemble.
//!
//! # Where the numbers come from
//!
//! All of it is read out of binutils by `tools/tables/arm.py`, which writes
//! [`super::cpu_data`]: the bit numbering and the architecture, CPU, unit and
//! extension sets from `include/opcode/arm.h` and the option tables of
//! `gas/config/tc-arm.c`, and what each instruction needs from GNU as's own
//! `insns[]` row for its mnemonic, which is the very set GNU as tests.
//!
//! One `insns[]` row covers every element size of a vector mnemonic, and GNU
//! as's encoder then checks the unit by hand, so a form of
//! [`super::table::FORMS`] carries a second set as well: the unit the
//! `opcodes/arm-dis.c` row it came from names, which is what tells the NEON
//! `vadd.i32 q0, q1, q2` from the VFP `vadd.f32 s0, s1, s2`. See
//! [`supports_form`].

use super::cpu_data::{
    ARCHS, CPUS, DEFAULT_ARCH, DEFAULT_FPU, EXTENSIONS, FEATS, FPU_ANY, FPUS, MNEMONICS, NAMES,
};
use crate::arch::{ArchState, CpuOption};

/// A feature set in GNU as's own numbering, its four 32-bit words packed two
/// to a `u64`: `core[0]` and `core[1]` in the first, `core[2]` and the
/// coprocessor word in the second.
pub type Set = [u64; 2];

/// The [`FEATS`] index of an instruction set that has no such instruction:
/// GNU as's null variant pointer, whose empty set nothing can match.
pub(crate) const ABSENT: u16 = 0;

/// The index [`mnemonic_feats`] gives a spelling GNU as reaches some way
/// other than an `insns[]` row of its own, and so has nothing to gate it
/// with.
pub(crate) const UNGATED: u16 = u16::MAX;

/// One `.arch` or `.cpu` name, with the sets GNU as's table gives it.
pub struct Named {
    pub key: &'static str,
    /// The architecture's or CPU's own features, GNU as's `V` column.
    pub set: Set,
    /// A CPU's extensions on top of that, its `E` column; nothing for an
    /// architecture.
    pub ext: Set,
    /// The unit it brings where no `.fpu` or `-mfpu=` names one, its `DF`
    /// column.
    pub fpu: Set,
    /// The `+name` extensions it has of its own, which GNU as keeps in an
    /// `arm_ext_table` and consults before the shared table.
    pub exts: &'static [Ext],
}

/// One extension name, and what adding it or taking it away changes.
pub struct Ext {
    pub key: &'static str,
    pub merge: Set,
    pub clear: Set,
    /// The architectures GNU as's shared `arm_extensions` table allows this
    /// name on: the architecture selected has to have every bit of one of
    /// them. Empty for an entry of an architecture's own table, which being
    /// there allows.
    pub allowed: &'static [Set],
}

const fn union(a: Set, b: Set) -> Set {
    [a[0] | b[0], a[1] | b[1]]
}

const fn without(a: Set, b: Set) -> Set {
    [a[0] & !b[0], a[1] & !b[1]]
}

/// Whether the two sets have a bit in common: `ARM_CPU_HAS_FEATURE`, which is
/// how GNU as decides an instruction is available.
const fn any(a: Set, b: Set) -> bool {
    a[0] & b[0] != 0 || a[1] & b[1] != 0
}

/// Whether `a` is a subset of `b`: `ARM_FSET_CPU_SUBSET`, which is how an
/// extension's `allowed_archs` is read.
const fn subset(a: Set, b: Set) -> bool {
    a[0] & b[0] == a[0] && a[1] & b[1] == a[1]
}

// How the selection is packed into `ArchState::cpu_features`. The first two
// words hold the extensions added since the architecture was chosen, the one
// part with no bound on which bits it may hold; the third holds the indices
// of the architecture and the unit, which are table entries rather than
// bits.
const EXT_LO: usize = 0;
const EXT_HI: usize = 1;
const WHICH: usize = 2;
/// The unit selected, one more than its [`FPUS`] index so that zero can mean
/// that none has been, in which case the architecture's own applies.
const FPU_SHIFT: u32 = 0;
const FPU_MASK: u64 = 0xff;
/// Set when `.fpu` chose the unit rather than `-mfpu=`: the directive takes
/// every unit bit away from the selection first, and the option does not.
const FPU_REPLACES: u64 = 1 << 8;
/// The architecture or CPU selected, one more than its index into [`ARCHS`]
/// followed by [`CPUS`], so that zero can mean the backend's own default.
const CPU_SHIFT: u32 = 16;
const CPU_MASK: u64 = 0xfff;

// Both indices are one more than the entry's, so a wider table than the
// field holds would read as another entry rather than fail to build.
const _: () = assert!(FPUS.len() < FPU_MASK as usize);
const _: () = assert!(ARCHS.len() + CPUS.len() < CPU_MASK as usize);

fn selected_index(state: &ArchState) -> usize {
    match ((state.cpu_features[WHICH] >> CPU_SHIFT) & CPU_MASK) as usize {
        0 => DEFAULT_ARCH,
        n => n - 1,
    }
}

fn entry(idx: usize) -> &'static Named {
    if idx < ARCHS.len() {
        &ARCHS[idx]
    } else {
        &CPUS[idx - ARCHS.len()]
    }
}

/// The name the target was selected by, for a diagnostic.
pub(crate) fn selected_name(state: &ArchState) -> &'static str {
    entry(selected_index(state)).key
}

fn added(state: &ArchState) -> Set {
    [state.cpu_features[EXT_LO], state.cpu_features[EXT_HI]]
}

/// The architecture or CPU's own features together with the extensions
/// added: GNU as's `selected_cpu`.
fn selected_cpu(state: &ArchState) -> Set {
    let whole = union(entry(selected_index(state)).set, added(state));
    // `.fpu` clears every unit bit from the selection before the unit's own
    // are added, which is how `.arch armv7-a` with `.arch_extension simd`
    // and then `.fpu vfpv2` loses NEON where the same with `-mfpu=vfpv2`
    // keeps it.
    if state.cpu_features[WHICH] & FPU_REPLACES != 0 {
        without(whole, FPU_ANY)
    } else {
        whole
    }
}

/// The unit selected: GNU as's `selected_fpu`.
fn selected_fpu(state: &ArchState) -> Set {
    match ((state.cpu_features[WHICH] >> FPU_SHIFT) & FPU_MASK) as usize {
        0 => entry(selected_index(state)).fpu,
        n => FPUS[n - 1].1,
    }
}

/// Everything the target has: GNU as's `cpu_variant`.
pub(crate) fn variant(state: &ArchState) -> Set {
    union(selected_cpu(state), selected_fpu(state))
}

/// The three words rsasm's ARM backend starts with; see the module note.
pub(crate) fn initial() -> [u64; 3] {
    let mut out = [0u64; 3];
    out[WHICH] = ((DEFAULT_ARCH as u64 + 1) << CPU_SHIFT)
        | ((DEFAULT_FPU as u64 + 1) << FPU_SHIFT)
        | FPU_REPLACES;
    out
}

/// Whether the selection is every core feature there is, which is what
/// `-march=all` and `-mcpu=all` name: `ARM_CPU_IS_ANY`, under which GNU as
/// leaves an obsolete instruction as a remark rather than refusing it.
pub(crate) fn is_any(state: &ArchState) -> bool {
    // Only the core words, as GNU as compares them.
    let selected = selected_cpu(state);
    selected[0] == super::cpu_data::ANY[0]
        && selected[1] & 0xffff_ffff == super::cpu_data::ANY[1] & 0xffff_ffff
}

/// Whether a 32-bit Thumb encoding of this mnemonic is available.
///
/// A mnemonic with both a 16-bit and a 32-bit Thumb encoding only has the
/// wide one from Thumb-2 on; one that has never had anything but a wide
/// encoding — the `bl` pair, the status-register transfers, the barriers,
/// the load/store exclusives and the Thumb divide — has it wherever the
/// mnemonic itself is available. That is GNU as's `t32_insn_ok`, which
/// reads the same answer off the mnemonic's own feature bits.
pub(crate) fn wide_ok(state: &ArchState, feats: u16, always_wide: bool) -> bool {
    always_wide
        || (feats != UNGATED && any(FEATS[feats as usize], super::cpu_data::WIDE_ONLY))
        || has(state, super::cpu_data::V6T2)
}

/// Whether the target has these features, for the handful of places a
/// hand-written encoder asks about one by name because GNU as's own encoder
/// does.
pub(crate) fn has(state: &ArchState, want: Set) -> bool {
    any(variant(state), want)
}

/// Whether the target has an instruction whose feature set is `idx`.
pub(crate) fn supports(state: &ArchState, idx: u16) -> bool {
    idx == UNGATED || (idx != ABSENT && any(variant(state), FEATS[idx as usize]))
}

/// Whether the target has one form of the generated table: the mnemonic's
/// own features, and the unit the form's row names where it names one. GNU
/// as makes the two checks in two places, the second in the encoder.
pub(crate) fn supports_form(state: &ArchState, feats: u16, unit: u16) -> bool {
    supports(state, feats) && (unit == ABSENT || supports(state, unit))
}

/// What an instruction needs, in words: the name of every bit of its set,
/// any one of which is enough.
fn describe(idx: u16) -> String {
    let want = FEATS[idx as usize];
    let names: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| any(*bit, want))
        .map(|&(_, name)| name)
        .collect();
    match names.len() {
        0 => "a processor this backend has no name for".to_string(),
        1 => names[0].to_string(),
        // The names read as "ARMv6K or later", so the list needs its own
        // comma: "ARMv6K or later, or Thumb-2".
        n => format!("{}, or {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

/// The diagnostic for an instruction the target does not have. GNU as says
/// "selected processor does not support `clz r0,r1' in ARM mode" and leaves
/// which processor would to the reader; this says which, as the m68k backend
/// does.
pub(crate) fn unsupported(state: &ArchState, name: &str, idx: u16, thumb: bool) -> String {
    let mode = if thumb { "Thumb" } else { "ARM" };
    if idx == ABSENT {
        return format!("`{name}` has no {mode} encoding");
    }
    format!(
        "`{name}` needs {}; this target is {} in {mode} mode",
        describe(idx),
        selected_name(state)
    )
}

/// What a mnemonic a hand-written encoder owns needs in the instruction set
/// `thumb` names: the [`FEATS`] index of its `insns[]` row, [`ABSENT`] where
/// GNU as has no such instruction there, or [`UNGATED`] where it has no row
/// under this spelling at all.
pub(crate) fn mnemonic_feats(name: &str, thumb: bool) -> u16 {
    // The type suffix a vector instruction carries is part of the spelling
    // here and not in GNU as's table, whose row covers every element size.
    for key in [name, name.split('.').next().unwrap_or(name)] {
        if let Ok(i) = MNEMONICS.binary_search_by(|(n, _, _)| (*n).cmp(key)) {
            let (_, arm, t) = MNEMONICS[i];
            return if thumb { t } else { arm };
        }
    }
    UNGATED
}

/// `directive` passes over the `all` entry each table starts with, which
/// `s_arm_arch` and `s_arm_cpu` do and the options do not: `-march=all` is
/// every core feature there is and `.arch all` is an unknown architecture.
fn set_arch(state: &mut ArchState, name: &str, directive: bool) -> bool {
    let skip = usize::from(directive);
    match ARCHS[skip..].iter().position(|a| a.key == name) {
        Some(i) => {
            set_selected(state, skip + i);
            true
        }
        None => false,
    }
}

fn set_cpu(state: &mut ArchState, name: &str, directive: bool) -> bool {
    let skip = usize::from(directive);
    match CPUS[skip..].iter().position(|c| c.key == name) {
        Some(i) => {
            set_selected(state, ARCHS.len() + skip + i);
            true
        }
        None => false,
    }
}

/// Selecting an architecture or CPU starts the extensions afresh: an
/// architecture's at none, a CPU's at the ones it comes with, which is where
/// GNU as starts `selected_ext` and so what a later `no<name>` can take
/// away.
fn set_selected(state: &mut ArchState, idx: usize) {
    let e = entry(idx);
    state.cpu_features[EXT_LO] = e.ext[0];
    state.cpu_features[EXT_HI] = e.ext[1];
    let keep = state.cpu_features[WHICH] & !(CPU_MASK << CPU_SHIFT);
    state.cpu_features[WHICH] = keep | ((idx as u64 + 1) << CPU_SHIFT);
}

fn set_fpu(state: &mut ArchState, name: &str, replaces: bool) -> bool {
    match FPUS.iter().position(|(key, _)| *key == name) {
        Some(i) => {
            state.cpu_features[WHICH] &= !((FPU_MASK << FPU_SHIFT) | FPU_REPLACES);
            state.cpu_features[WHICH] |=
                ((i as u64 + 1) << FPU_SHIFT) | if replaces { FPU_REPLACES } else { 0 };
            true
        }
        None => false,
    }
}

/// One `+name`, `+noname` or `.arch_extension` name applied to the
/// selection. `from` is where to start looking in the shared table, which
/// the option path advances and the directive leaves at zero; `Ok(None)`
/// means neither table has the name.
fn set_extension(state: &mut ArchState, name: &str, from: usize) -> Result<Option<usize>, String> {
    let (name, adding) = match name.strip_prefix("no") {
        Some(rest) if !rest.is_empty() => (rest, false),
        _ => (name, true),
    };
    // GNU as passes over an entry of an architecture's own table that has
    // nothing to say in this direction — an `ARM_ADD` has nothing to clear —
    // and goes on to the shared one.
    let own = entry(selected_index(state)).exts;
    if let Some(e) = own
        .iter()
        .find(|e| e.key == name && if adding { e.merge } else { e.clear } != [0, 0])
    {
        apply_extension(state, e, adding);
        return Ok(Some(from));
    }
    let Some(at) = EXTENSIONS[from..].iter().position(|e| e.key == name) else {
        return Ok(None);
    };
    let e = &EXTENSIONS[from + at];
    if !e
        .allowed
        .iter()
        .any(|a| subset(*a, entry(selected_index(state)).set))
    {
        // Taking away what the architecture never had is a remark in GNU as
        // rather than an error, and changes nothing either way.
        return if adding {
            Err(format!(
                "`{name}` is not an extension of {}",
                selected_name(state)
            ))
        } else {
            Ok(Some(from + at + 1))
        };
    }
    apply_extension(state, e, adding);
    Ok(Some(from + at + 1))
}

fn apply_extension(state: &mut ArchState, e: &Ext, adding: bool) {
    let next = if adding {
        union(added(state), e.merge)
    } else {
        without(added(state), e.clear)
    };
    state.cpu_features[EXT_LO] = next[0];
    state.cpu_features[EXT_HI] = next[1];
}

/// `.arch` and `.cpu`, which take a bare name: `s_arm_arch` and `s_arm_cpu`
/// compare the whole of it, so neither reads the `+name` suffixes the
/// matching option does.
pub(crate) fn directive_arch(state: &mut ArchState, name: &str, cpu: bool) -> bool {
    if cpu {
        set_cpu(state, name, true)
    } else {
        set_arch(state, name, true)
    }
}

/// `.fpu`.
pub(crate) fn directive_fpu(state: &mut ArchState, name: &str) -> bool {
    set_fpu(state, name, true)
}

/// `.arch_extension`, which reads the shared table from the start every
/// time. `Ok(false)` is a name neither table has.
pub(crate) fn directive_extension(state: &mut ArchState, name: &str) -> Result<bool, String> {
    Ok(set_extension(state, name, 0)?.is_some())
}

/// `-march=`, `-mcpu=` and `-mfpu=`, the first two of which may follow the
/// name with `+name` and `+noname` extension suffixes. `arm_parse_fpu`
/// compares the whole of its argument, so a unit takes none.
pub(crate) fn option(state: &mut ArchState, opt: CpuOption, arg: &str) -> Result<(), String> {
    let (name, exts) = match arg.split_once('+') {
        Some((name, rest)) if opt != CpuOption::Fpu => (name, Some(rest)),
        _ => (arg, None),
    };
    let ok = match opt {
        CpuOption::Cpu => set_cpu(state, name, false),
        CpuOption::Fpu => set_fpu(state, name, false),
        _ => set_arch(state, name, false),
    };
    if !ok {
        return Err(match opt {
            CpuOption::Cpu => format!("unknown cpu `{name}`"),
            CpuOption::Fpu => format!("unknown floating point format `{name}`"),
            _ => format!("unknown architecture `{name}`"),
        });
    }
    // GNU as reads the shared table forwards without going back, which is
    // why it asks for the suffixes in alphabetical order, and starts it over
    // once the first `no` name begins the removals.
    let mut from = 0;
    let mut removing = false;
    for ext in exts.into_iter().flat_map(|r| r.split('+')) {
        if ext.is_empty() {
            return Err("missing architectural extension".to_string());
        }
        if !removing && ext.len() >= 3 && ext.starts_with("no") {
            removing = true;
            from = 0;
        }
        match set_extension(state, ext, from)? {
            Some(next) => from = next,
            None if EXTENSIONS
                .iter()
                .any(|e| e.key == ext.trim_start_matches("no")) =>
            {
                return Err(
                    "architectural extensions must be specified in alphabetical order".to_string(),
                );
            }
            None => return Err(format!("unknown architectural extension `{ext}`")),
        }
    }
    Ok(())
}
