//! The one table of target features the tiers read (INTRINSICS.md 3.1, P8): per arch, each
//! feature's Rust name (what `#[target_feature(enable = "..")]` and `cfg(target_feature)` say),
//! its LLVM names (what an LLVM `"target-features"` string says) and what it implies directly;
//! a set of them as one [`Features`] bit set; and each target's base set, the features its
//! default CPU has.
//!
//! SOURCES (pinned nightly `rustc 1.101.0-nightly (16d1ffac1 2026-10-26)`, LLVM 23.1.4):
//! - The rows and their `implies` are rustc's own tables as crates/target-features/src/lib.rs
//!   holds them (`AARCH64_FEATURES`, `X86_FEATURES`, copied from `rustc_target`), every row but
//!   the `InternalOnly` ones, which `#[target_feature]` refuses (they are in [`INTERNAL_AARCH64`]
//!   and [`INTERNAL_X86_64`] and refused by name). tests/features.rs `table_is_rustcs` checks
//!   the rows against that crate's `TargetArch::rustc_table`, row by row.
//! - The LLVM names are rustc's `to_llvm_features` for LLVM 23 as
//!   crates/hir-ty-codegen/target.rs `renamed_for_llvm` records them (`fp16` is `fullfp16`,
//!   `neon` is `neon` with `fp-armv8` tied to it, `sse4.2` turns `crc32` on, `tme` has no LLVM
//!   name); a row the function does not name has its Rust name.
//! - The base sets are the `target_feature` cfgs the pinned rustc prints for each target with no
//!   `-C target-cpu` and no `-C target-feature`, as recorded in
//!   pins/answers/rustc.json (`targets.<triple>.cfg` and `.nightly_cfg`):
//!   aarch64-apple-darwin's spec says `"cpu": "apple-m1"` (`target_spec_json`), and its 34
//!   features equal crates/target-features/src/cpus/table.rs's set of `apple-m1` (= `apple-a14`).
//!   Not from memory: the planner's list in INTRINSICS.md 2.6 (`sha3, fp16, ..`) is a summary.
//!
//! A [`Features`] is a set over ONE arch's table: bit `i` is row `i` of [`table`]. Sets of two
//! arches never mix: [`Features::union`] and [`Features::is_subset`] refuse that by a panic (a
//! caller's bug, never data), so a set is correct by construction.
//!
//! No allocation, no recursion: the implication closure of every row is computed once, at
//! compile time, by a fixpoint over bit sets.

use core::fmt;

use crate::regs::{Arch, Os};

/// One target feature of one arch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Feature {
    /// Its Rust name: `sha3`, `fp16`, `avx2`.
    pub rust: &'static str,
    /// Its LLVM names, comma-separated, as enabling it turns them on: `fullfp16`;
    /// `neon,fp-armv8`; empty where this LLVM has none (`tme`). See [`Feature::llvm_names`].
    pub llvm: &'static str,
    /// The Rust names of the features it implies directly (rustc's table's third column).
    pub implies: &'static [&'static str],
}

impl Feature {
    /// Its LLVM names, one by one, in the row's order; none for a feature this LLVM lacks.
    pub fn llvm_names(&self) -> impl Iterator<Item = &'static str> {
        self.llvm.split(',').filter(|name| !name.is_empty())
    }
}

const fn f(rust: &'static str, llvm: &'static str, implies: &'static [&'static str]) -> Feature {
    Feature { rust, llvm, implies }
}

/// The most rows one arch's table may hold: a [`Features`] is a `u128`.
pub const MAX_FEATURES: usize = 128;

/// rustc's AArch64 table (target-features `AARCH64_FEATURES`), in its order, without its two
/// `InternalOnly` rows (`fp-armv8`, `reserve-x18`).
const AARCH64: &[Feature] = &[
    f("aes", "aes", &["neon"]),
    f("bf16", "bf16", &[]),
    f("bti", "bti", &[]),
    f("crc", "crc", &[]),
    f("cssc", "cssc", &[]),
    f("dit", "dit", &[]),
    f("dotprod", "dotprod", &["neon"]),
    f("dpb", "ccpp", &[]),
    f("dpb2", "ccdp", &["dpb"]),
    f("ecv", "ecv", &[]),
    f("f32mm", "f32mm", &["sve"]),
    f("f64mm", "f64mm", &["sve"]),
    f("faminmax", "faminmax", &[]),
    f("fcma", "complxnum", &["neon"]),
    f("fhm", "fp16fml", &["fp16"]),
    f("flagm", "flagm", &[]),
    f("flagm2", "altnzcv", &[]),
    f("fp8", "fp8", &["faminmax", "lut", "bf16"]),
    f("fp8dot2", "fp8dot2", &["fp8dot4"]),
    f("fp8dot4", "fp8dot4", &["fp8fma"]),
    f("fp8fma", "fp8fma", &["fp8"]),
    f("fp16", "fullfp16", &["neon"]),
    f("frintts", "fptoint", &[]),
    f("hbc", "hbc", &[]),
    f("i8mm", "i8mm", &[]),
    f("jsconv", "jsconv", &["neon"]),
    f("lor", "lor", &[]),
    f("lse", "lse", &[]),
    f("lse2", "lse2", &[]),
    f("lse128", "lse128", &["lse"]),
    f("lut", "lut", &[]),
    f("mops", "mops", &[]),
    f("mte", "mte", &[]),
    f("neon", "neon,fp-armv8", &[]),
    f("outline-atomics", "outline-atomics", &[]),
    f("paca", "pauth", &[]),
    f("pacg", "pauth", &[]),
    f("pan", "pan", &[]),
    f("pauth-lr", "pauth-lr", &[]),
    f("pmuv3", "perfmon", &[]),
    f("rand", "rand", &[]),
    f("ras", "ras", &[]),
    f("rcpc", "rcpc", &[]),
    f("rcpc2", "rcpc-immo", &["rcpc"]),
    f("rcpc3", "rcpc3", &["rcpc2"]),
    f("rdm", "rdm", &["neon"]),
    f("sb", "sb", &[]),
    f("sha2", "sha2", &["neon"]),
    f("sha3", "sha3", &["sha2"]),
    f("sm4", "sm4", &["neon"]),
    f("sme", "sme", &["bf16"]),
    f("sme-b16b16", "sme-b16b16", &["bf16", "sme2", "sve-b16b16"]),
    f("sme-f8f16", "sme-f8f16", &["sme-f8f32"]),
    f("sme-f8f32", "sme-f8f32", &["sme2", "fp8"]),
    f("sme-f16f16", "sme-f16f16", &["sme2"]),
    f("sme-f64f64", "sme-f64f64", &["sme"]),
    f("sme-fa64", "sme-fa64", &["sme", "sve2"]),
    f("sme-i16i64", "sme-i16i64", &["sme"]),
    f("sme-lutv2", "sme-lutv2", &[]),
    f("sme2", "sme2", &["sme"]),
    f("sme2p1", "sme2p1", &["sme2"]),
    f("spe", "spe", &[]),
    f("ssbs", "ssbs", &[]),
    f("ssve-fp8dot2", "ssve-fp8dot2", &["ssve-fp8dot4"]),
    f("ssve-fp8dot4", "ssve-fp8dot4", &["ssve-fp8fma"]),
    f("ssve-fp8fma", "ssve-fp8fma", &["sme2", "fp8"]),
    f("sve", "sve", &["neon", "fp16"]),
    f("sve-b16b16", "sve-b16b16", &["bf16"]),
    f("sve2", "sve2", &["sve"]),
    f("sve2-aes", "sve2-aes", &["sve2", "aes"]),
    f("sve2-bitperm", "sve2-bitperm", &["sve2"]),
    f("sve2-sha3", "sve2-sha3", &["sve2", "sha3"]),
    f("sve2-sm4", "sve2-sm4", &["sve2", "sm4"]),
    f("sve2p1", "sve2p1", &["sve2"]),
    f("tme", "", &[]),
    f("v8.1a", "v8.1a", &["crc", "lse", "rdm", "pan", "lor", "vh"]),
    f("v8.2a", "v8.2a", &["v8.1a", "ras", "dpb"]),
    f("v8.3a", "v8.3a", &["v8.2a", "rcpc", "paca", "pacg", "jsconv"]),
    f("v8.4a", "v8.4a", &["v8.3a", "dotprod", "dit", "flagm"]),
    f("v8.5a", "v8.5a", &["v8.4a", "ssbs", "sb", "dpb2", "bti"]),
    f("v8.6a", "v8.6a", &["v8.5a", "bf16", "i8mm"]),
    f("v8.7a", "v8.7a", &["v8.6a", "wfxt"]),
    f("v8.8a", "v8.8a", &["v8.7a", "hbc", "mops"]),
    f("v8.9a", "v8.9a", &["v8.8a", "cssc"]),
    f("v9.1a", "v9.1a", &["v9a", "v8.6a"]),
    f("v9.2a", "v9.2a", &["v9.1a", "v8.7a"]),
    f("v9.3a", "v9.3a", &["v9.2a", "v8.8a"]),
    f("v9.4a", "v9.4a", &["v9.3a", "v8.9a"]),
    f("v9.5a", "v9.5a", &["v9.4a"]),
    f("v9a", "v9a", &["v8.5a"]),
    f("vh", "vh", &[]),
    f("wfxt", "wfxt", &[]),
];

/// rustc's x86 table (target-features `X86_FEATURES`), in its order, without its four
/// `InternalOnly` rows (`retpoline-*`, `soft-float`).
const X86_64: &[Feature] = &[
    f("adx", "adx", &[]),
    f("aes", "aes", &["sse2"]),
    f("amx-avx512", "amx-avx512", &["amx-tile"]),
    f("amx-bf16", "amx-bf16", &["amx-tile"]),
    f("amx-complex", "amx-complex", &["amx-tile"]),
    f("amx-fp8", "amx-fp8", &["amx-tile"]),
    f("amx-fp16", "amx-fp16", &["amx-tile"]),
    f("amx-int8", "amx-int8", &["amx-tile"]),
    f("amx-movrs", "amx-movrs", &["amx-tile"]),
    f("amx-tile", "amx-tile", &[]),
    f("apxf", "egpr,push2pop2,ppx,ndd,ccmp,cf,nf,zu", &[]),
    f("avx", "avx", &["sse4.2"]),
    f("avx2", "avx2", &["avx"]),
    f(
        "avx10.1",
        "avx10.1",
        &[
            "avx512bf16",
            "avx512bitalg",
            "avx512bw",
            "avx512cd",
            "avx512dq",
            "avx512f",
            "avx512fp16",
            "avx512ifma",
            "avx512vbmi",
            "avx512vbmi2",
            "avx512vl",
            "avx512vnni",
            "avx512vpopcntdq",
        ],
    ),
    f("avx10.2", "avx10.2", &["avx10.1", "avxvnni", "avxvnniint8", "avxvnniint16"]),
    f("avx512bf16", "avx512bf16", &["avx512bw"]),
    f("avx512bitalg", "avx512bitalg", &["avx512bw"]),
    f("avx512bw", "avx512bw", &["avx512f"]),
    f("avx512cd", "avx512cd", &["avx512f"]),
    f("avx512dq", "avx512dq", &["avx512f"]),
    f("avx512f", "avx512f", &["avx2", "fma", "f16c"]),
    f("avx512fp16", "avx512fp16", &["avx512bw"]),
    f("avx512ifma", "avx512ifma", &["avx512f"]),
    f("avx512vbmi", "avx512vbmi", &["avx512bw"]),
    f("avx512vbmi2", "avx512vbmi2", &["avx512bw"]),
    f("avx512vl", "avx512vl", &["avx512f"]),
    f("avx512vnni", "avx512vnni", &["avx512f"]),
    f("avx512vp2intersect", "avx512vp2intersect", &["avx512f"]),
    f("avx512vpopcntdq", "avx512vpopcntdq", &["avx512f"]),
    f("avxifma", "avxifma", &["avx2"]),
    f("avxneconvert", "avxneconvert", &["avx2"]),
    f("avxvnni", "avxvnni", &["avx2"]),
    f("avxvnniint8", "avxvnniint8", &["avx2"]),
    f("avxvnniint16", "avxvnniint16", &["avx2"]),
    f("bmi1", "bmi", &[]),
    f("bmi2", "bmi2", &[]),
    f("clflushopt", "clflushopt", &[]),
    f("cmpxchg16b", "cx16", &[]),
    f("ermsb", "ermsb", &[]),
    f("f16c", "f16c", &["avx"]),
    f("fma", "fma", &["avx"]),
    f("fma4", "fma4", &["avx", "sse4a"]),
    f("fxsr", "fxsr", &[]),
    f("gfni", "gfni", &["sse2"]),
    f("kl", "kl", &["sse2"]),
    f("lahfsahf", "sahf", &[]),
    f("lzcnt", "lzcnt", &[]),
    f("movbe", "movbe", &[]),
    f("movdir64b", "movdir64b", &[]),
    f("movdiri", "movdiri", &[]),
    f("movrs", "movrs", &[]),
    f("pclmulqdq", "pclmul", &["sse2"]),
    f("popcnt", "popcnt", &[]),
    f("prfchw", "prfchw", &[]),
    f("rdrand", "rdrnd", &[]),
    f("rdseed", "rdseed", &[]),
    f("rtm", "rtm", &[]),
    f("sha", "sha", &["sse2"]),
    f("sha512", "sha512", &["avx2"]),
    f("sm3", "sm3", &["avx"]),
    f("sm4", "sm4", &["avx2"]),
    f("sse", "sse", &[]),
    f("sse2", "sse2", &["sse"]),
    f("sse3", "sse3", &["sse2"]),
    f("sse4.1", "sse4.1", &["ssse3"]),
    f("sse4.2", "sse4.2,crc32", &["sse4.1"]),
    f("sse4a", "sse4a", &["sse3"]),
    f("ssse3", "ssse3", &["sse3"]),
    f("tbm", "tbm", &[]),
    f("vaes", "vaes", &["avx2", "aes"]),
    f("vpclmulqdq", "vpclmulqdq", &["avx", "pclmulqdq"]),
    f("widekl", "widekl", &["kl"]),
    f("x87", "x87", &[]),
    f("xop", "xop", &["fma4", "avx", "sse4a"]),
    f("xsave", "xsave", &[]),
    f("xsavec", "xsavec", &["xsave"]),
    f("xsaveopt", "xsaveopt", &["xsave"]),
    f("xsaves", "xsaves", &["xsave"]),
];

/// rustc's AArch64 rows `#[target_feature]` refuses (`Stability::InternalOnly`), with rustc's
/// reason.
pub const INTERNAL_AARCH64: &[(&str, &str)] = &[
    ("fp-armv8", "Rust ties `fp-armv8` to `neon`"),
    ("reserve-x18", "use `-Zfixed-x18` compiler flag instead"),
];

/// rustc's x86 rows `#[target_feature]` refuses (`Stability::InternalOnly`), with rustc's
/// reason.
pub const INTERNAL_X86_64: &[(&str, &str)] = &[
    ("retpoline-external-thunk", "use `-Zretpoline-external-thunk` compiler flag instead"),
    ("retpoline-indirect-branches", "use `-Zretpoline` compiler flag instead"),
    ("retpoline-indirect-calls", "use `-Zretpoline` compiler flag instead"),
    ("soft-float", "use a soft-float target instead"),
];

const _: () = assert!(AARCH64.len() <= MAX_FEATURES && X86_64.len() <= MAX_FEATURES);

/// The feature table of `arch`, in rustc's order: bit `i` of a [`Features`] of `arch` is row `i`.
pub fn table(arch: Arch) -> &'static [Feature] {
    match arch {
        Arch::Arm64 => AARCH64,
        Arch::X86_64 => X86_64,
    }
}

const fn internal(arch: Arch) -> &'static [(&'static str, &'static str)] {
    match arch {
        Arch::Arm64 => INTERNAL_AARCH64,
        Arch::X86_64 => INTERNAL_X86_64,
    }
}

const fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The row of `rust` in `rows`, by its Rust name.
const fn row_of(rows: &[Feature], rust: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < rows.len() {
        if same(rows[i].rust.as_bytes(), rust) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Per row: itself and everything it implies, transitively. A name a row implies that the
/// table lacks fails the build (a const panic), never a quiet gap.
const fn closures(rows: &[Feature]) -> [u128; MAX_FEATURES] {
    let mut closed = [0u128; MAX_FEATURES];
    let mut i = 0;
    while i < rows.len() {
        let mut bits = 1u128 << i;
        let implies = rows[i].implies;
        let mut k = 0;
        while k < implies.len() {
            match row_of(rows, implies[k].as_bytes()) {
                Some(j) => bits |= 1u128 << j,
                None => panic!("a feature implies a name its table does not have"),
            }
            k += 1;
        }
        closed[i] = bits;
        i += 1;
    }
    // Fixpoint: each pass ORs in what every member implies; it ends when a pass changes
    // nothing, at most once per row (each changing pass adds a bit to some row).
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < rows.len() {
            let mut bits = closed[i];
            let mut j = 0;
            while j < rows.len() {
                if closed[i] & (1u128 << j) != 0 {
                    bits |= closed[j];
                }
                j += 1;
            }
            if bits != closed[i] {
                closed[i] = bits;
                changed = true;
            }
            i += 1;
        }
        if !changed {
            break;
        }
    }
    closed
}

static AARCH64_CLOSED: [u128; MAX_FEATURES] = closures(AARCH64);
static X86_64_CLOSED: [u128; MAX_FEATURES] = closures(X86_64);

fn closed(arch: Arch) -> &'static [u128; MAX_FEATURES] {
    match arch {
        Arch::Arm64 => &AARCH64_CLOSED,
        Arch::X86_64 => &X86_64_CLOSED,
    }
}

/// The set of the rows named in `names`; a name the table lacks fails the build.
const fn named(rows: &[Feature], names: &[&str]) -> u128 {
    let mut bits = 0u128;
    let mut k = 0;
    while k < names.len() {
        match row_of(rows, names[k].as_bytes()) {
            Some(i) => bits |= 1u128 << i,
            None => panic!("a base feature its table does not have"),
        }
        k += 1;
    }
    bits
}

/// aarch64-apple-darwin, CPU `apple-m1`: rustc.json `targets."aarch64-apple-darwin"`, the
/// `target_feature` entries of `cfg` (30) and of `nightly_cfg` (`flagm2`, `lse2`, `v8.1a` ..
/// `v8.4a`), sorted.
const BASE_ARM64_MACOS: &[&str] = &[
    "aes", "crc", "dit", "dotprod", "dpb", "dpb2", "fcma", "fhm", "flagm", "flagm2", "fp16", "frintts", "jsconv",
    "lor", "lse", "lse2", "neon", "paca", "pacg", "pan", "pmuv3", "ras", "rcpc", "rcpc2", "rdm", "sb", "sha2", "sha3",
    "ssbs", "v8.1a", "v8.2a", "v8.3a", "v8.4a", "vh",
];

/// aarch64-unknown-linux-gnu, CPU `generic`: rustc.json's `cfg` for it.
const BASE_ARM64_LINUX: &[&str] = &["neon", "outline-atomics"];

/// x86_64-apple-darwin: rustc.json's `cfg` and `nightly_cfg` for it.
const BASE_X86_64_MACOS: &[&str] =
    &["cmpxchg16b", "fxsr", "lahfsahf", "sse", "sse2", "sse3", "sse4.1", "ssse3", "x87"];

/// x86_64-unknown-linux-gnu: rustc.json's `cfg` and `nightly_cfg` for it.
const BASE_X86_64_LINUX: &[&str] = &["fxsr", "sse", "sse2", "x87"];

const BASE: [[u128; 2]; 2] = [
    [named(AARCH64, BASE_ARM64_MACOS), named(AARCH64, BASE_ARM64_LINUX)],
    [named(X86_64, BASE_X86_64_MACOS), named(X86_64, BASE_X86_64_LINUX)],
];

/// The features the target's default CPU has, as the pinned rustc gives them with no
/// `-C target-cpu` and no `-C target-feature` (see the module's sources).
pub fn base(arch: Arch, os: Os) -> Features {
    let a = match arch {
        Arch::Arm64 => 0,
        Arch::X86_64 => 1,
    };
    let o = match os {
        Os::MacOs => 0,
        Os::Linux => 1,
    };
    Features { arch, bits: BASE[a][o] }
}

/// Enables `list`, the form of `#[target_feature(enable = "..")]`: Rust names separated by
/// `,` with nothing else around them; each brings what it implies, transitively. A name the
/// table lacks, a name rustc keeps internal and an empty name are refused, each by name.
pub fn parse(arch: Arch, list: &str) -> Result<Features, FeatureError> {
    let rows = table(arch);
    let closed = closed(arch);
    let mut bits = 0u128;
    let mut at = 0usize;
    for name in list.split(',') {
        if name.is_empty() {
            return Err(FeatureError::Empty { arch, at: u32::try_from(at).unwrap_or(u32::MAX) });
        }
        match row_of(rows, name.as_bytes()) {
            Some(i) => bits |= closed[i],
            None => {
                if let Some(&(name, reason)) = internal(arch).iter().find(|(it, _)| *it == name) {
                    return Err(FeatureError::Internal { arch, name, reason });
                }
                return Err(FeatureError::Unknown { arch, name: FeatureName::of(name) });
            }
        }
        at += name.len() + 1;
    }
    Ok(Features { arch, bits })
}

/// A set of one arch's features: bit `i` is row `i` of [`table`]. Part of a record key
/// wherever it decides code (INTRINSICS.md 3.3: a function's `target_feature` set).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Features {
    arch: Arch,
    bits: u128,
}

impl Features {
    /// The empty set of `arch`.
    pub const fn empty(arch: Arch) -> Features {
        Features { arch, bits: 0 }
    }

    pub const fn arch(self) -> Arch {
        self.arch
    }

    /// The bits, for a key's bytes: bit `i` is row `i` of `table(self.arch())`.
    pub const fn bits(self) -> u128 {
        self.bits
    }

    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Whether the set holds the feature Rust calls `rust`; `None` when the arch's table has
    /// no such name.
    pub fn contains(self, rust: &str) -> Option<bool> {
        row_of(table(self.arch), rust.as_bytes()).map(|i| self.bits & (1u128 << i) != 0)
    }

    /// Both sets. Panics on two arches: a caller's bug, never data.
    pub fn union(self, other: Features) -> Features {
        assert_eq!(self.arch, other.arch, "a union of two arches' feature sets");
        Features { arch: self.arch, bits: self.bits | other.bits }
    }

    /// Whether every feature of `self` is in `other`. Panics on two arches.
    pub fn is_subset(self, other: Features) -> bool {
        assert_eq!(self.arch, other.arch, "a comparison of two arches' feature sets");
        self.bits & !other.bits == 0
    }

    /// The set with everything its members imply, transitively.
    pub fn closed(self) -> Features {
        let closed = closed(self.arch);
        let mut bits = self.bits;
        let mut rest = self.bits;
        while rest != 0 {
            let i = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            bits |= closed[i];
        }
        Features { arch: self.arch, bits }
    }

    /// The rows in the set, in table order.
    pub fn iter(self) -> impl Iterator<Item = &'static Feature> {
        table(self.arch).iter().enumerate().filter(move |(i, _)| self.bits & (1u128 << i) != 0).map(|(_, row)| row)
    }

    /// The LLVM names the set turns on, in table order; a name may come twice (`paca` and
    /// `pacg` are both `pauth`).
    pub fn llvm_names(self) -> impl Iterator<Item = &'static str> {
        self.iter().flat_map(Feature::llvm_names)
    }
}

/// A name refused, kept inline (no allocation): its first [`FeatureName::KEPT`] bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FeatureName {
    bytes: [u8; FeatureName::KEPT],
    len: u8,
    /// The name was longer than what is kept.
    cut: bool,
}

impl FeatureName {
    pub const KEPT: usize = 48;

    fn of(name: &str) -> FeatureName {
        // Cut at a char boundary so the kept bytes stay UTF-8.
        let mut end = name.len().min(Self::KEPT);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; Self::KEPT];
        bytes[..end].copy_from_slice(&name.as_bytes()[..end]);
        FeatureName { bytes, len: end as u8, cut: end < name.len() }
    }

    /// The kept text.
    pub fn as_str(&self) -> &str {
        // The bytes are a prefix of a `&str` cut at a char boundary.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Debug for FeatureName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}{}", self.as_str(), if self.cut { "…" } else { "" })
    }
}

impl fmt::Display for FeatureName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}{}`", self.as_str(), if self.cut { "…" } else { "" })
    }
}

/// Why a feature list was refused. Every variant names the arch and the name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeatureError {
    /// rustc's table of the arch has no feature of this name.
    Unknown { arch: Arch, name: FeatureName },
    /// rustc keeps this feature internal (`Stability::InternalOnly`): `#[target_feature]` may
    /// not name it, for `reason`.
    Internal { arch: Arch, name: &'static str, reason: &'static str },
    /// An empty name at byte `at` of the list (`a,,b`, a trailing `,`, an empty list).
    Empty { arch: Arch, at: u32 },
}

impl fmt::Display for FeatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureError::Unknown { arch, name } => write!(
                f,
                "{name} is no target feature of {arch:?} (rustc's table, crates/blaze-tier/src/features.rs)"
            ),
            FeatureError::Internal { arch, name, reason } => {
                write!(f, "`{name}` is a target feature of {arch:?} rustc keeps internal: {reason}")
            }
            FeatureError::Empty { arch, at } => {
                write!(f, "an empty target feature name at byte {at} of a {arch:?} feature list")
            }
        }
    }
}

impl core::error::Error for FeatureError {}
