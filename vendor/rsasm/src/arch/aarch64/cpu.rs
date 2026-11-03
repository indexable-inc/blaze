//! Which instructions the target selected has, and the check every
//! instruction goes through.
//!
//! GNU as keeps one `aarch64_feature_set` — 148 bits, three 64-bit words —
//! and lets an instruction through only when the target has *every* bit the
//! instruction names (`AARCH64_CPU_HAS_ALL_FEATURES`). That is the opposite
//! of the ARM backend's test, where any one bit will do, and it is why a
//! diagnostic here can name what is missing: `casp` wants `lse`, and a
//! `bfmmla` wants `sve2` and `bf16` together.
//!
//! What selects it is `.arch`, `.cpu` and `.arch_extension`, or `-march=`
//! and `-mcpu=` with their `+name` and `+noname` suffixes. The set is not
//! simply the union of what was asked for: GNU as closes it over the
//! dependencies in its own table both ways — adding `+sve2` adds `sve`, and
//! `+nosve` takes `sve2` away with it — turns on the features that stand for
//! a combination of others, and lets `+sme` imply `sve` and `sve2` whether
//! or not a `+nosve` followed. This module does all four, out of the tables
//! in [`super::cpu_data`].
//!
//! # The default
//!
//! A source that names nothing gets [`DEFAULT`]: every extension this
//! backend has an instruction for, which is the `-march=` `tools/xas-diff`
//! and `tools/mc-diff` assemble the two references with and so the target
//! every AArch64 corpus here was measured against. GNU as's own default for
//! `aarch64-elf` is `armv8-a`, and llvm-mc's is an `armv8-a` of its own;
//! rsasm does not follow either, because then every corpus line that uses
//! SVE, SME or an extension would have to say `.arch` first, and none of
//! them was measured that way.
//!
//! # Where the numbers come from
//!
//! `tools/tables/a64feat.py` reads the bit numbering, the architecture
//! versions, the CPUs, the extensions and their dependencies out of
//! binutils, and with them the feature set of every instruction in GNU as's
//! opcode table. A mnemonic [`super::insn`] encodes by hand carries the bits
//! every row of that name needs, which is the most a name alone can say; a
//! form of the generated table carries the set of the one row that encodes
//! it, found by its opcode, which is finer — `fmmla` wants `+f32mm` for
//! single precision and `+f64mm` for double. The named operands of the
//! system instructions carry one too; see [`super::sysreg`].

use super::cpu_data::{
    ARCHS, CPUS, DEFAULT, DEPENDENCIES, EXTENSIONS, FEATS, MNEMONICS, NAMES, SME, SVE_SVE2,
};
use crate::arch::{ArchState, CpuOption};

/// A feature set in GNU as's own numbering: 148 bits in three words.
pub type Set = [u64; 3];

/// No features at all, which is `AARCH64_NO_FEATURES`.
const NONE: Set = [0, 0, 0];

/// One `+name` extension: what it turns on, and what has to be on with it.
pub struct Ext {
    pub key: &'static str,
    pub value: Set,
    /// GNU as's `require`: the features this one is defined in terms of, and
    /// so cannot be had without. Adding this extension adds them, and taking
    /// any of them away takes this one away too.
    pub require: Set,
}

const fn union(a: Set, b: Set) -> Set {
    [a[0] | b[0], a[1] | b[1], a[2] | b[2]]
}

const fn without(a: Set, b: Set) -> Set {
    [a[0] & !b[0], a[1] & !b[1], a[2] & !b[2]]
}

/// Whether `set` has every bit of `want`: `AARCH64_CPU_HAS_ALL_FEATURES`,
/// the test an instruction goes through.
const fn has_all(set: Set, want: Set) -> bool {
    !set[0] & want[0] == 0 && !set[1] & want[1] == 0 && !set[2] & want[2] == 0
}

/// Whether they have any bit in common: `AARCH64_CPU_HAS_ANY_FEATURES`.
const fn has_any(a: Set, b: Set) -> bool {
    a[0] & b[0] != 0 || a[1] & b[1] != 0 || a[2] & b[2] != 0
}

/// Everything in `want` and everything it depends on:
/// `aarch64_feature_enable_set`, with the special case that `+sme` brings
/// `+sve2` whatever else was asked for.
fn enable_set(want: Set) -> Set {
    let mut want = want;
    let mut prev = NONE;
    while !has_all(prev, want) {
        prev = want;
        for e in EXTENSIONS {
            if has_all(want, e.value) {
                want = union(want, e.require);
            }
        }
        if has_all(want, SME) {
            want = union(want, SVE_SVE2);
        }
    }
    want
}

/// Everything in `want` and everything that depends on it:
/// `aarch64_feature_disable_set`, which is what `+noname` takes away.
fn disable_set(want: Set) -> Set {
    let mut want = want;
    let mut prev = NONE;
    while !has_all(prev, want) {
        prev = want;
        for e in EXTENSIONS {
            if has_any(e.require, want) {
                want = union(want, e.value);
            }
        }
    }
    want
}

/// The features that stand for a combination of others, recomputed:
/// `aarch64_update_virtual_dependencies`. They are the opcode table's way of
/// saying "SVE2 or streaming SVE", and no option names them.
fn virtual_dependencies(set: Set) -> Set {
    let mut set = set;
    for (_test, enables) in DEPENDENCIES {
        set = without(set, *enables);
    }
    for (test, enables) in DEPENDENCIES {
        if has_all(set, *test) {
            set = union(set, *enables);
        }
    }
    set
}

/// The target's features.
fn variant(state: &ArchState) -> Set {
    state.cpu_features
}

/// The three words rsasm's AArch64 backend starts with; see the module note.
pub(crate) fn initial() -> Set {
    DEFAULT
}

/// Whether the target has an instruction whose feature set is `idx`.
pub(crate) fn supports(state: &ArchState, feats: Set) -> bool {
    has_all(variant(state), feats)
}

/// What a mnemonic [`super::insn`] encodes by hand needs: the feature set
/// every row of GNU as's opcode table under that name has in common, which
/// is the most a mnemonic alone can say.
pub(crate) fn mnemonic_feats(name: &str) -> Set {
    match MNEMONICS.binary_search_by(|(n, _)| (*n).cmp(name)) {
        Ok(i) => FEATS[MNEMONICS[i].1 as usize],
        // A spelling only rsasm has, which nothing here gates.
        Err(_) => NONE,
    }
}

/// The diagnostic for an instruction the target does not have: the `+name`
/// of every feature it wants and the target has not. GNU as says "selected
/// processor does not support `casp x0, x1, x2, x3, [x4]'" and leaves which
/// extension to the reader.
pub(crate) fn unsupported(state: &ArchState, name: &str, feats: Set) -> String {
    let missing = without(feats, variant(state));
    let names: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| has_any(*bit, missing))
        .map(|&(_, n)| n)
        .collect();
    let what = match names.len() {
        0 => "another processor".to_string(),
        1 => format!("`+{}`", names[0]),
        n => format!(
            "{} and `+{}`",
            names[..n - 1]
                .iter()
                .map(|n| format!("`+{n}`"))
                .collect::<Vec<_>>()
                .join(", "),
            names[n - 1]
        ),
    };
    format!("`{name}` needs {what}, which this target has not")
}

/// One name from a `+`-separated list, applied to `set`.
fn one_extension(set: Set, name: &str) -> Result<Set, String> {
    let (key, adding) = match name.strip_prefix("no") {
        Some(rest) if !rest.is_empty() => (rest, false),
        _ => (name, true),
    };
    let Some(e) = EXTENSIONS.iter().find(|e| e.key == key) else {
        return Err(format!("unknown architectural extension `{name}`"));
    };
    Ok(if adding {
        union(set, enable_set(e.value))
    } else {
        without(set, disable_set(e.value))
    })
}

/// GNU as's `aarch64_parse_features`: a base set and a `+`-separated list of
/// extensions to add and then to take away.
///
/// `ext_only` is how `.arch_extension` reads it, which takes a single name
/// with no `+` in front of it; the options want one `+` before each.
pub(crate) fn parse_features(base: Set, list: &str, ext_only: bool) -> Result<Set, String> {
    let mut set = base;
    if list.is_empty() {
        return Ok(virtual_dependencies(set));
    }
    if ext_only {
        set = one_extension(set, list)?;
    } else {
        // Every name has to be introduced by a `+`, and once one has been
        // taken away no more may be added, which GNU as insists on so that a
        // target string reads in one direction.
        let mut rest = list;
        let mut removing = false;
        while !rest.is_empty() {
            let Some(tail) = rest.strip_prefix('+') else {
                return Err("invalid architectural extension".to_string());
            };
            let end = tail.find('+').unwrap_or(tail.len());
            let name = &tail[..end];
            if name.is_empty() {
                return Err("missing architectural extension".to_string());
            }
            let takes_away = name.len() >= 3 && name.starts_with("no");
            if removing && !takes_away {
                return Err(
                    "must specify extensions to add before specifying those to remove".to_string(),
                );
            }
            removing |= takes_away;
            set = one_extension(set, name)?;
            rest = &tail[end..];
        }
    }
    // The last word on SVE: many streaming-SVE instructions are still marked
    // as needing SVE or SVE2, so GNU as puts both back whenever SME is on,
    // which lets `+sme+nosve` mean what a compiler passing it expects.
    if has_all(set, SME) {
        set = union(set, SVE_SVE2);
    }
    Ok(virtual_dependencies(set))
}

/// `.arch` and `.cpu`, and `-march=` and `-mcpu=`: a name from one of the
/// two tables, with extension suffixes after it. `directive` says to pass
/// over the `all` entry, which `s_aarch64_arch` and `s_aarch64_cpu` do and
/// the options do not.
fn select(state: &mut ArchState, cpu: bool, arg: &str, directive: bool) -> Result<(), String> {
    let (name, exts) = match arg.find('+') {
        Some(at) => (&arg[..at], &arg[at..]),
        None => (arg, ""),
    };
    let table: &[(&str, Set)] = if cpu { CPUS } else { ARCHS };
    let skip = usize::from(directive);
    let Some(&(_, base)) = table[skip..].iter().find(|(key, _)| *key == name) else {
        return Err(if cpu {
            format!("unknown cpu `{name}`")
        } else {
            format!("unknown architecture `{name}`")
        });
    };
    state.cpu_features = parse_features(base, exts, false)?;
    Ok(())
}

/// Whether `.arch` or `.cpu` naming this is one of GNU as's AArch64
/// architectures or CPUs rather than another backend.
pub(crate) fn knows(name: &str, cpu: bool) -> bool {
    let table: &[(&str, Set)] = if cpu { CPUS } else { ARCHS };
    let base = name.split('+').next().unwrap_or(name);
    table[1..].iter().any(|(key, _)| *key == base)
}

/// `.arch` and `.cpu`.
pub(crate) fn directive_arch(state: &mut ArchState, name: &str, cpu: bool) -> Result<(), String> {
    select(state, cpu, name, true)
}

/// `.arch_extension`, which takes one name, with `no` in front of it to take
/// the extension away.
pub(crate) fn directive_extension(state: &mut ArchState, name: &str) -> Result<(), String> {
    state.cpu_features = parse_features(state.cpu_features, name, true)?;
    Ok(())
}

/// `-march=` and `-mcpu=`.
pub(crate) fn option(state: &mut ArchState, opt: CpuOption, arg: &str) -> Result<(), String> {
    match opt {
        CpuOption::Cpu => select(state, true, arg, false),
        CpuOption::Arch => select(state, false, arg, false),
        _ => Err(format!(
            "`{}` is not an option of the aarch64 backend",
            opt.flag()
        )),
    }
}
