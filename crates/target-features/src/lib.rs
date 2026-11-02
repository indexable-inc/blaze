//! rustc's target feature tables, below `cfg` and the front end, which name features by them.
//!
//! Every name of rustc's per-arch tables below is a [`TargetFeature`], a `u16` index into
//! [`NAMES`]; a set of them is a [`TargetFeatureSet`], one bit each. Name bytes find their
//! feature by a perfect hash built by `const fn`, and what each feature implies, transitively,
//! is a `const`-built bitset per entry of each arch's table, found through a `u16` per feature
//! and arch: a feature implies what its target's table says, as in rustc (`aes` is `neon` on
//! AArch64 and `sse2` on x86). Nothing is interned, hashed or allocated to ask whether a
//! feature is enabled.

#![forbid(unsafe_code)]

use std::fmt;

use blaze_ids::bits::{BitArray, BitIndex};

#[cfg(test)]
mod tests;

/// One name of rustc's target feature tables (of every arch: `aes` of Arm, AArch64 and x86 is
/// one feature): its index in [`NAMES`], so features order as their names' bytes do.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetFeature(u16);

impl TargetFeature {
    /// The number of target features.
    pub const COUNT: usize = COUNT;

    pub const AVX512BF16: Self = Self::named("avx512bf16");
    pub const AVX512F: Self = Self::named("avx512f");
    pub const AVX512FP16: Self = Self::named("avx512fp16");
    pub const AVX512VL: Self = Self::named("avx512vl");
    pub const AVXNECONVERT: Self = Self::named("avxneconvert");
    pub const AVXVNNI: Self = Self::named("avxvnni");
    pub const F16C: Self = Self::named("f16c");
    pub const FMA: Self = Self::named("fma");
    pub const FMA4: Self = Self::named("fma4");
    pub const FP16: Self = Self::named("fp16");
    pub const OUTLINE_ATOMICS: Self = Self::named("outline-atomics");
    pub const RELAXED_SIMD: Self = Self::named("relaxed-simd");
    pub const SIMD128: Self = Self::named("simd128");

    /// The feature named `name`, if a table has it: one hash of its bytes, two loads and one
    /// compare.
    #[inline]
    pub fn from_name(name: &str) -> Option<Self> {
        BY_NAME_TABLE.get(name).map(|at| Self(at as u16))
    }

    /// The feature named `name`, in a `const`: a name no table has fails to compile.
    pub const fn named(name: &str) -> Self {
        match BY_NAME.index_const(name.as_bytes()) {
            Some(at) => Self(at as u16),
            None => panic!("not a target feature of rustc's tables"),
        }
    }

    #[inline]
    pub fn name(self) -> &'static str {
        NAMES[self.index()]
    }

    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Every target feature, in the order of their names' bytes.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..COUNT).map(|at| Self(at as u16))
    }

    /// The feature and every feature it implies on `arch`, transitively: what rustc's
    /// `implied_target_features` gives from the target's own table. A feature `arch`'s table
    /// does not name implies nothing.
    #[inline]
    pub fn implied_in(self, arch: TargetArch) -> TargetFeatureSet {
        match arch.entry_of(self) {
            Some(entry) => IMPLIED[entry],
            None => TargetFeatureSet::EMPTY.with(self),
        }
    }
}

/// The architecture whose table says what a target feature implies, as rustc's
/// `Target::rust_target_features` picks one from `target_arch`: one variant per table, in the
/// order of `TARGET_FEATURE_IMPLICATIONS_RAW`, and `Other` for a target without one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum TargetArch {
    Arm,
    AArch64,
    X86,
    Hexagon,
    PowerPC,
    Mips,
    Nvptx,
    RiscV,
    Wasm,
    Bpf,
    Xtensa,
    Csky,
    LoongArch,
    IbmZ,
    Sparc,
    M68k,
    Avr,
    Other,
}

const _: () = assert!(TargetArch::Other as usize == TARGET_FEATURE_IMPLICATIONS_RAW.len());

impl TargetArch {
    /// The arch of a `target_arch` cfg value.
    pub fn from_target_arch(name: &str) -> Self {
        match name {
            "arm" => Self::Arm,
            "aarch64" | "arm64ec" => Self::AArch64,
            "x86" | "x86_64" => Self::X86,
            "hexagon" => Self::Hexagon,
            "mips" | "mips32r6" | "mips64" | "mips64r6" => Self::Mips,
            "nvptx64" => Self::Nvptx,
            "powerpc" | "powerpc64" => Self::PowerPC,
            "riscv32" | "riscv64" => Self::RiscV,
            "wasm32" | "wasm64" => Self::Wasm,
            "bpf" => Self::Bpf,
            "csky" => Self::Csky,
            "loongarch32" | "loongarch64" => Self::LoongArch,
            "s390x" => Self::IbmZ,
            "sparc" | "sparc64" => Self::Sparc,
            "m68k" => Self::M68k,
            "avr" => Self::Avr,
            "xtensa" => Self::Xtensa,
            _ => Self::Other,
        }
    }

    /// rustc's own table of this arch (`rustc_target`'s), each line's name and what it implies
    /// directly, in the table's order, a name possibly twice; none for [`TargetArch::Other`].
    /// What the sets built from it are checked against.
    pub fn rustc_table(self) -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
        let table = TARGET_FEATURE_IMPLICATIONS_RAW.get(self as usize).copied().unwrap_or(&[]);
        table.iter().map(|&(name, _, implied)| (name, implied))
    }

    /// The entry of `feature` in this arch's table, the last when it names it twice (rustc
    /// reads the table into a map).
    #[inline]
    fn entry_of(self, feature: TargetFeature) -> Option<usize> {
        let table = ENTRY_OF.get(self as usize)?;
        match table[feature.index()] {
            NO_ENTRY => None,
            entry => Some(entry as usize),
        }
    }
}

impl fmt::Debug for TargetFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl BitIndex for TargetFeature {
    #[inline]
    fn to_bit(self) -> usize {
        self.index()
    }

    #[inline]
    fn from_bit(bit: usize) -> Self {
        Self(<u16 as BitIndex>::from_bit(bit))
    }
}

/// A set of target features, one bit each: 72 bytes, a test is one load and a shift. A struct
/// of its own for a `Debug` that names the features.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TargetFeatureSet(BitArray<WORDS, TargetFeature>);

const _: () = assert!(size_of::<TargetFeatureSet>() == 72);

impl TargetFeatureSet {
    pub const EMPTY: Self = Self(BitArray::<WORDS, TargetFeature>::EMPTY);

    pub const fn of(features: &[TargetFeature]) -> Self {
        let mut set = Self::EMPTY;
        let mut i = 0;
        while i < features.len() {
            set = set.with(features[i]);
            i += 1;
        }
        set
    }

    #[inline]
    pub const fn with(self, feature: TargetFeature) -> Self {
        Self(self.0.with_bit(feature.index()))
    }

    #[inline]
    pub fn insert(&mut self, feature: TargetFeature) {
        self.0.insert(feature);
    }

    #[inline]
    pub const fn contains(&self, feature: TargetFeature) -> bool {
        self.0.has_bit(feature.index())
    }

    #[inline]
    pub const fn union(&self, other: &Self) -> Self {
        Self(self.0.or(&other.0))
    }

    /// Whether every feature of `other` is in `self`.
    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        other.0.is_subset(&self.0)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The set and every feature its features imply on `arch`, transitively.
    pub fn implied_in(&self, arch: TargetArch) -> Self {
        self.iter().fold(*self, |out, feature| out.union(&feature.implied_in(arch)))
    }

    /// The features, in the order of their names' bytes.
    pub fn iter(&self) -> impl Iterator<Item = TargetFeature> + '_ {
        self.0.iter()
    }

    /// The names of the features, sorted by their bytes.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.iter().map(TargetFeature::name)
    }

    const fn same(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.0.array(), other.0.array());
        let mut i = 0;
        while i < WORDS {
            if ours[i] != theirs[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl FromIterator<TargetFeature> for TargetFeatureSet {
    fn from_iter<I: IntoIterator<Item = TargetFeature>>(iter: I) -> Self {
        let mut set = Self::EMPTY;
        iter.into_iter().for_each(|feature| set.insert(feature));
        set
    }
}

impl fmt::Debug for TargetFeatureSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.names()).finish()
    }
}

/// Every name of the tables below, once, sorted by bytes: a feature's index is its place here.
/// A name of a table missing here fails to compile; one here no table has fails a test.
#[rustfmt::skip]
const NAMES: &[&str] = &[
    "10e60", "2e3", "32s", "3e3r1", "3e3r2", "3e3r3", "3e7", "7e10", "a", "aclass",
    "acquire-release", "addsubiw", "adx", "aes", "allows-misaligned-mem-access", "altivec", "alu32",
    "amx-avx512", "amx-bf16", "amx-complex", "amx-fp16", "amx-fp8", "amx-int8", "amx-movrs",
    "amx-tile", "apxf", "atomics", "atomics-32", "audio", "avx", "avx10.1", "avx10.2", "avx2",
    "avx512bf16", "avx512bitalg", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512fp16",
    "avx512ifma", "avx512vbmi", "avx512vbmi2", "avx512vl", "avx512vnni", "avx512vp2intersect",
    "avx512vpopcntdq", "avxifma", "avxneconvert", "avxvnni", "avxvnniint16", "avxvnniint8", "b",
    "backchain", "bf16", "bmi1", "bmi2", "bool", "break", "bti", "bulk-memory", "c", "cache",
    "clflushopt", "cmpxchg16b", "concurrent-functions", "coprocessor", "crc", "cssc", "d", "d32",
    "debug", "deflate-conversion", "dit", "div32", "doloop", "dotprod", "dpb", "dpb2", "dsp",
    "dsp1e2", "dspe60", "e", "e1", "e2", "ecv", "edsp", "eijmpcall", "elpm", "elpmx", "elrw",
    "enhanced-sort", "ermsb", "exception", "exception-handling", "extended-const", "extendedl32r",
    "f", "f16c", "f32mm", "f64mm", "faminmax", "fcma", "fdivdu", "fhm", "flagm", "flagm2",
    "float1e2", "float1e3", "float3e4", "float7e60", "floate1", "fma", "fma4", "forced-atomics",
    "fp", "fp-armv8", "fp16", "fp64", "fp8", "fp8dot2", "fp8dot4", "fp8fma", "fpregs", "fpuv2_df",
    "fpuv2_sf", "fpuv3_df", "fpuv3_hf", "fpuv3_hi", "fpuv3_sf", "frecipe", "frintts", "fxsr", "gc",
    "gfni", "guarded-storage", "hard-float", "hard-float-abi", "hard-tp", "hbc", "high-registers",
    "high-word", "highpriinterrupts", "hvx", "hvx-ieee-fp", "hvx-length128b", "hvx-length64b",
    "hvx-qfloat", "hvxv60", "hvxv62", "hvxv65", "hvxv66", "hvxv67", "hvxv68", "hvxv69", "hvxv71",
    "hvxv73", "hvxv75", "hvxv79", "hwdiv", "i8mm", "ijmpcall", "interrupt", "isa-68000",
    "isa-68010", "isa-68020", "isa-68030", "isa-68040", "isa-68060", "isa-68881", "isa-68882",
    "jmpcall", "jsconv", "kl", "lahfsahf", "lam-bh", "lamcas", "lasx", "lbt", "ld-seq-sa",
    "leoncasa", "loop", "lor", "lowbytefirst", "lpm", "lpmx", "lse", "lse128", "lse2", "lsx", "lut",
    "lvz", "lzcnt", "m", "mac16", "mclass", "message-security-assist-extension12",
    "message-security-assist-extension3", "message-security-assist-extension4",
    "message-security-assist-extension5", "message-security-assist-extension8",
    "message-security-assist-extension9", "miscellaneous-extensions-2",
    "miscellaneous-extensions-3", "miscellaneous-extensions-4", "miscsr", "mops", "movbe",
    "movdir64b", "movdiri", "movrs", "movw", "mp", "mp1e2", "msa", "msync", "mte", "mul", "mul32",
    "mul32high", "multivalue", "mutable-globals", "mve", "mve.fp", "neon", "nnp-assist",
    "nontrapping-fptoint", "nsa", "nvic", "outline-atomics", "paca", "pacg", "pan",
    "partword-atomics", "pauth-lr", "pclmulqdq", "pmuv3", "popcnt", "power10-vector",
    "power8-altivec", "power8-crypto", "power8-vector", "power9-altivec", "power9-vector", "prfchw",
    "prid", "ptx70", "ptx71", "ptx72", "ptx73", "ptx74", "ptx75", "ptx76", "ptx77", "ptx78",
    "ptx80", "ptx81", "ptx82", "ptx83", "ptx84", "ptx85", "ptx86", "ptx87", "quadword-atomics",
    "rand", "ras", "rclass", "rcpc", "rcpc2", "rcpc3", "rdm", "rdrand", "rdseed", "reference-types",
    "regprotect", "relax", "relaxed-simd", "reserve-x18", "retpoline-external-thunk",
    "retpoline-indirect-branches", "retpoline-indirect-calls", "rmw", "rtm", "rva23u64", "rvector",
    "s32c1i", "sb", "scq", "sext", "sha", "sha2", "sha3", "sha512", "sign-ext", "simd128", "sm3",
    "sm4", "sm_100", "sm_100a", "sm_101", "sm_101a", "sm_120", "sm_120a", "sm_70", "sm_72", "sm_75",
    "sm_80", "sm_86", "sm_87", "sm_89", "sm_90", "sm_90a", "sme", "sme-b16b16", "sme-f16f16",
    "sme-f64f64", "sme-f8f16", "sme-f8f32", "sme-fa64", "sme-i16i64", "sme-lutv2", "sme2", "sme2p1",
    "soft-float", "spe", "spm", "spmx", "sram", "ssbs", "sse", "sse2", "sse3", "sse4.1", "sse4.2",
    "sse4a", "ssse3", "ssve-fp8dot2", "ssve-fp8dot4", "ssve-fp8fma", "supm", "sve", "sve-b16b16",
    "sve2", "sve2-aes", "sve2-bitperm", "sve2-sha3", "sve2-sm4", "sve2p1", "tail-call", "tbm",
    "threadptr", "thumb-mode", "thumb2", "tinyencoding", "tme", "transactional-execution", "trust",
    "trustzone", "ual", "unaligned-scalar-mem", "unaligned-vector-mem", "v", "v5te", "v6", "v60",
    "v62", "v65", "v66", "v67", "v68", "v69", "v6k", "v6m", "v6t2", "v7", "v71", "v73", "v75",
    "v79", "v8", "v8.1a", "v8.1m.main", "v8.2a", "v8.3a", "v8.4a", "v8.5a", "v8.6a", "v8.7a",
    "v8.8a", "v8.9a", "v8m", "v8m.main", "v8plus", "v9", "v9.1a", "v9.2a", "v9.3a", "v9.4a",
    "v9.5a", "v9a", "vaes", "vdsp2e60f", "vdspv1", "vdspv2", "vector", "vector-enhancements-1",
    "vector-enhancements-2", "vector-enhancements-3", "vector-packed-decimal",
    "vector-packed-decimal-enhancement", "vector-packed-decimal-enhancement-2",
    "vector-packed-decimal-enhancement-3", "vfp2", "vfp2sp", "vfp3", "vfp4", "vh", "virt",
    "virtualization", "vpclmulqdq", "vsx", "wfxt", "wide-arithmetic", "widekl", "windowed", "x87",
    "xop", "xsave", "xsavec", "xsaveopt", "xsaves", "za128rs", "za64rs", "zaamo", "zabha", "zacas",
    "zalrsc", "zama16b", "zawrs", "zba", "zbb", "zbc", "zbkb", "zbkc", "zbkx", "zbs", "zca", "zcb",
    "zcmop", "zdinx", "zfa", "zfbfmin", "zfh", "zfhmin", "zfinx", "zhinx", "zhinxmin", "zic64b",
    "zicbom", "zicbop", "zicboz", "ziccamoa", "ziccif", "zicclsm", "ziccrse", "zicntr", "zicond",
    "zicsr", "zifencei", "zihintntl", "zihintpause", "zihpm", "zimop", "zk", "zkn", "zknd", "zkne",
    "zkne_or_zknd", "zknh", "zkr", "zks", "zksed", "zksh", "zkt", "zmmul", "zreg", "ztso", "zvbb",
    "zvbc", "zve32f", "zve32x", "zve64d", "zve64f", "zve64x", "zvfbfmin", "zvfbfwma", "zvfh",
    "zvfhmin", "zvkb", "zvkg", "zvkn", "zvknc", "zvkned", "zvkng", "zvknha", "zvknhb", "zvks",
    "zvksc", "zvksed", "zvksg", "zvksh", "zvkt", "zvl1024b", "zvl128b", "zvl16384b", "zvl2048b",
    "zvl256b", "zvl32768b", "zvl32b", "zvl4096b", "zvl512b", "zvl64b", "zvl65536b", "zvl8192b",
];

const COUNT: usize = NAMES.len();
const WORDS: usize = COUNT.div_ceil(64);

const _: () = assert!(COUNT <= u16::MAX as usize);

type ByName = blaze_phf::Phf<{ blaze_phf::slots(COUNT) }, { blaze_phf::buckets(COUNT) }>;
/// For building the tables below in `const`; run time reads [`BY_NAME_TABLE`].
const BY_NAME: ByName = ByName::new(NAMES);
static BY_NAME_TABLE: ByName = BY_NAME;

/// The index of each feature of every table and of each it implies, all of `NAMES`.
const _: () = {
    let mut arch = 0;
    while arch < TARGET_FEATURE_IMPLICATIONS_RAW.len() {
        let table = TARGET_FEATURE_IMPLICATIONS_RAW[arch];
        let mut i = 0;
        while i < table.len() {
            TargetFeature::named(table[i].0);
            i += 1;
        }
        arch += 1;
    }
};

const EDGE_COUNT: usize = {
    let mut count = 0;
    let mut arch = 0;
    while arch < TARGET_FEATURE_IMPLICATIONS_RAW.len() {
        let table = TARGET_FEATURE_IMPLICATIONS_RAW[arch];
        let mut i = 0;
        while i < table.len() {
            count += table[i].2.len();
            i += 1;
        }
        arch += 1;
    }
    count
};

const ARCHES: usize = TARGET_FEATURE_IMPLICATIONS_RAW.len();

/// The entries of every table, one per line of each arch's table, archs in order.
const ENTRY_COUNT: usize = {
    let mut count = 0;
    let mut arch = 0;
    while arch < ARCHES {
        count += TARGET_FEATURE_IMPLICATIONS_RAW[arch].len();
        arch += 1;
    }
    count
};

const _: () = assert!(ENTRY_COUNT < NO_ENTRY as usize);

/// The feature of each entry and the arch of its table.
const ENTRIES: [(u16, u8); ENTRY_COUNT] = {
    let mut entries = [(0, 0); ENTRY_COUNT];
    let mut at = 0;
    let mut arch = 0;
    while arch < ARCHES {
        let table = TARGET_FEATURE_IMPLICATIONS_RAW[arch];
        let mut i = 0;
        while i < table.len() {
            entries[at] = (TargetFeature::named(table[i].0).0, arch as u8);
            at += 1;
            i += 1;
        }
        arch += 1;
    }
    entries
};

/// No entry: the arch's table does not name the feature.
const NO_ENTRY: u16 = u16::MAX;

/// For each arch, each feature's entry in its table, the later one of a name the table has
/// twice: 2 bytes a feature, so finding a row is one load.
const ENTRY_OF_TABLE: [[u16; COUNT]; ARCHES] = {
    let entries = ENTRIES;
    let mut table = [[NO_ENTRY; COUNT]; ARCHES];
    let mut at = 0;
    while at < ENTRY_COUNT {
        let (feature, arch) = entries[at];
        table[arch as usize][feature as usize] = at as u16;
        at += 1;
    }
    table
};
static ENTRY_OF: [[u16; COUNT]; ARCHES] = ENTRY_OF_TABLE;

/// Each implication of every table: an entry and a feature it implies directly, in that
/// entry's arch.
const EDGES: [(u16, u16); EDGE_COUNT] = {
    let mut edges = [(0, 0); EDGE_COUNT];
    let mut at = 0;
    let mut entry = 0;
    let mut arch = 0;
    while arch < ARCHES {
        let table = TARGET_FEATURE_IMPLICATIONS_RAW[arch];
        let mut i = 0;
        while i < table.len() {
            let implied = table[i].2;
            let mut j = 0;
            while j < implied.len() {
                edges[at] = (entry as u16, TargetFeature::named(implied[j]).0);
                at += 1;
                j += 1;
            }
            entry += 1;
            i += 1;
        }
        arch += 1;
    }
    edges
};

/// Each entry's feature and every feature it implies in its arch, transitively: rows made the
/// union of the rows of what they imply until none grows, which handles a cycle (`e2` implies
/// itself). A feature the table does not name implies only itself. 72 bytes an entry.
static IMPLIED: [TargetFeatureSet; ENTRY_COUNT] = {
    // Locals, so that the loop indexes them in place rather than a copy of each table.
    let (entries, entry_of, edges) = (ENTRIES, ENTRY_OF_TABLE, EDGES);
    let mut rows = [TargetFeatureSet::EMPTY; ENTRY_COUNT];
    let mut at = 0;
    while at < ENTRY_COUNT {
        rows[at] = rows[at].with(TargetFeature(entries[at].0));
        at += 1;
    }
    let mut changed = true;
    while changed {
        changed = false;
        let mut e = 0;
        while e < EDGE_COUNT {
            let (from, to) = edges[e];
            let from = from as usize;
            let arch = entries[from].1 as usize;
            let merged = match entry_of[arch][to as usize] {
                NO_ENTRY => rows[from].with(TargetFeature(to)),
                to_entry => rows[from].union(&rows[to_entry as usize]),
            };
            if !merged.same(&rows[from]) {
                rows[from] = merged;
                changed = true;
            }
            e += 1;
        }
    }
    rows
};

#[expect(dead_code, reason = "stubs for rustc_target")]
enum Stability {
    Stable,
    Unstable(()),
    InternalOnly { reason: &'static str, hard_error: bool },
}
use Stability::*;

type ImpliedFeatures = &'static [&'static str];

// spellchecker:off

#[expect(non_upper_case_globals, reason = "stubs for rustc_target")]
mod sym {
    // ARM
    pub(super) const arm_target_feature: () = ();
    // AArch64
    pub(super) const aarch64_unstable_target_feature: () = ();
    pub(super) const aarch64_ver_target_feature: () = ();
    // x86
    pub(super) const x86_amx_intrinsics: () = ();
    pub(super) const apx_target_feature: () = ();
    pub(super) const avx10_target_feature: () = ();
    pub(super) const clflushopt_target_feature: () = ();
    pub(super) const fma4_target_feature: () = ();
    pub(super) const ermsb_target_feature: () = ();
    pub(super) const lahfsahf_target_feature: () = ();
    pub(super) const movrs_target_feature: () = ();
    pub(super) const prfchw_target_feature: () = ();
    pub(super) const rtm_target_feature: () = ();
    pub(super) const x87_target_feature: () = ();
    pub(super) const xop_target_feature: () = ();
    pub(super) const movdir64b_target_feature: () = ();
    pub(super) const movdiri_target_feature: () = ();
    // Hexagon
    pub(super) const hexagon_target_feature: () = ();
    // PowerPC
    pub(super) const powerpc_target_feature: () = ();
    // MIPS
    pub(super) const mips_target_feature: () = ();
    // NVPTX
    pub(super) const nvptx_target_feature: () = ();
    // RISC-V
    pub(super) const riscv_target_feature: () = ();
    // WASM
    pub(super) const wasm_target_feature: () = ();
    // BPF
    pub(super) const bpf_target_feature: () = ();
    // CSKY
    pub(super) const csky_target_feature: () = ();
    // LoongArch
    pub(super) const loongarch_target_feature: () = ();
    // s390x
    pub(super) const s390x_target_feature: () = ();
    // SPARC
    pub(super) const sparc_target_feature: () = ();
    // m68k
    pub(super) const m68k_target_feature: () = ();
    // AVR
    pub(super) const avr_target_feature: () = ();
    // Xtensa
    pub(super) const xtensa_target_feature: () = ();
}

// The following code is copied verbatim from compiler/rustc_target/src/target_features.rs.

const ARM_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("aclass", Unstable(sym::arm_target_feature), &[]),
    ("acquire-release", Unstable(sym::arm_target_feature), &[]),
    ("aes", Unstable(sym::arm_target_feature), &["neon"]),
    (
        "atomics-32",
        // Not implied by any CPU model or other feature.
        Stability::InternalOnly {
            reason: "unsound because it changes the ABI of atomic operations",
            hard_error: false,
        },
        &[],
    ),
    ("crc", Unstable(sym::arm_target_feature), &[]),
    ("d32", Unstable(sym::arm_target_feature), &[]),
    ("dotprod", Unstable(sym::arm_target_feature), &["neon"]),
    ("dsp", Unstable(sym::arm_target_feature), &[]),
    ("fp-armv8", Unstable(sym::arm_target_feature), &["vfp4"]),
    ("fp16", Unstable(sym::arm_target_feature), &["neon"]),
    ("fp64", Unstable(sym::arm_target_feature), &[]),
    ("fpregs", Unstable(sym::arm_target_feature), &[]),
    ("i8mm", Unstable(sym::arm_target_feature), &["neon"]),
    ("mclass", Unstable(sym::arm_target_feature), &[]),
    ("mve", Unstable(sym::arm_target_feature), &["v8.1m.main", "dsp", "fpregs"]),
    ("mve.fp", Unstable(sym::arm_target_feature), &["mve"]),
    ("neon", Unstable(sym::arm_target_feature), &["vfp3"]),
    ("rclass", Unstable(sym::arm_target_feature), &[]),
    ("sha2", Unstable(sym::arm_target_feature), &["neon"]),
    // This can be *disabled* on non-`hf` targets to enable the use
    // of hardfloats while keeping the softfloat ABI.
    // FIXME before stabilization: Should we expose this as a `hard-float` target feature instead of
    // matching the odd negative feature LLVM uses?
    ("soft-float", Unstable(sym::arm_target_feature), &[]),
    // This is needed for inline assembly, but shouldn't be stabilized as-is
    // since it should be enabled per-function using #[instruction_set], not
    // #[target_feature].
    ("thumb-mode", Unstable(sym::arm_target_feature), &[]),
    ("thumb2", Unstable(sym::arm_target_feature), &[]),
    ("trustzone", Unstable(sym::arm_target_feature), &[]),
    ("v5te", Unstable(sym::arm_target_feature), &[]),
    ("v6", Unstable(sym::arm_target_feature), &["v5te"]),
    ("v6k", Unstable(sym::arm_target_feature), &["v6"]),
    ("v6m", Unstable(sym::arm_target_feature), &["v6"]),
    ("v6t2", Unstable(sym::arm_target_feature), &["v6k", "v8m", "thumb2"]),
    ("v7", Unstable(sym::arm_target_feature), &["v6t2"]),
    ("v8", Unstable(sym::arm_target_feature), &["v7", "acquire-release"]),
    ("v8.1m.main", Unstable(sym::arm_target_feature), &["v8m.main"]),
    ("v8m", Unstable(sym::arm_target_feature), &["v6m"]),
    ("v8m.main", Unstable(sym::arm_target_feature), &["v7"]),
    ("vfp2", Unstable(sym::arm_target_feature), &["vfp2sp", "fp64"]),
    ("vfp2sp", Unstable(sym::arm_target_feature), &["fpregs"]),
    ("vfp3", Unstable(sym::arm_target_feature), &["vfp2", "d32"]),
    ("vfp4", Unstable(sym::arm_target_feature), &["vfp3"]),
    ("virtualization", Unstable(sym::arm_target_feature), &[]),
    // tidy-alphabetical-end
];

const AARCH64_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    // FEAT_AES & FEAT_PMULL
    ("aes", Stable, &["neon"]),
    // FEAT_BF16
    ("bf16", Stable, &[]),
    // FEAT_BTI
    ("bti", Stable, &[]),
    // FEAT_CRC
    ("crc", Stable, &[]),
    // FEAT_CSSC
    ("cssc", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_DIT
    ("dit", Stable, &[]),
    // FEAT_DotProd
    ("dotprod", Stable, &["neon"]),
    // FEAT_DPB
    ("dpb", Stable, &[]),
    // FEAT_DPB2
    ("dpb2", Stable, &["dpb"]),
    // FEAT_ECV
    ("ecv", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_F32MM
    ("f32mm", Stable, &["sve"]),
    // FEAT_F64MM
    ("f64mm", Stable, &["sve"]),
    // FEAT_FAMINMAX
    ("faminmax", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_FCMA
    ("fcma", Stable, &["neon"]),
    // FEAT_FHM
    ("fhm", Stable, &["fp16"]),
    // FEAT_FLAGM
    ("flagm", Stable, &[]),
    // FEAT_FLAGM2
    ("flagm2", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // We forbid directly toggling just `fp-armv8`; it must be toggled with `neon`.
    (
        "fp-armv8",
        // Pinned down by [`Target::abi_required_features`] when needed.
        Stability::InternalOnly { reason: "Rust ties `fp-armv8` to `neon`", hard_error: false },
        &[],
    ),
    // FEAT_FP8
    ("fp8", Unstable(sym::aarch64_unstable_target_feature), &["faminmax", "lut", "bf16"]),
    // FEAT_FP8DOT2
    ("fp8dot2", Unstable(sym::aarch64_unstable_target_feature), &["fp8dot4"]),
    // FEAT_FP8DOT4
    ("fp8dot4", Unstable(sym::aarch64_unstable_target_feature), &["fp8fma"]),
    // FEAT_FP8FMA
    ("fp8fma", Unstable(sym::aarch64_unstable_target_feature), &["fp8"]),
    // FEAT_FP16
    // Rust ties FP and Neon: https://github.com/rust-lang/rust/pull/91608
    ("fp16", Stable, &["neon"]),
    // FEAT_FRINTTS
    ("frintts", Stable, &[]),
    // FEAT_HBC
    ("hbc", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_I8MM
    ("i8mm", Stable, &[]),
    // FEAT_JSCVT
    // Rust ties FP and Neon: https://github.com/rust-lang/rust/pull/91608
    ("jsconv", Stable, &["neon"]),
    // FEAT_LOR
    ("lor", Stable, &[]),
    // FEAT_LSE
    ("lse", Stable, &[]),
    // FEAT_LSE2
    ("lse2", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_LSE128
    ("lse128", Unstable(sym::aarch64_unstable_target_feature), &["lse"]),
    // FEAT_LUT
    ("lut", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_MOPS
    ("mops", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_MTE & FEAT_MTE2
    ("mte", Stable, &[]),
    // FEAT_AdvSimd & FEAT_FP
    ("neon", Stable, &[]),
    // Backend option to turn atomic operations into an intrinsic call when `lse` is not known to be
    // available, so the intrinsic can do runtime LSE feature detection rather than unconditionally
    // using slower non-LSE operations. Unstable since it doesn't need to user-togglable.
    ("outline-atomics", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_PAUTH (address authentication)
    ("paca", Stable, &[]),
    // FEAT_PAUTH (generic authentication)
    ("pacg", Stable, &[]),
    // FEAT_PAN
    ("pan", Stable, &[]),
    // FEAT_PAuth_LR
    ("pauth-lr", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_PMUv3
    ("pmuv3", Stable, &[]),
    // FEAT_RNG
    ("rand", Stable, &[]),
    // FEAT_RAS & FEAT_RASv1p1
    ("ras", Stable, &[]),
    // FEAT_LRCPC
    ("rcpc", Stable, &[]),
    // FEAT_LRCPC2
    ("rcpc2", Stable, &["rcpc"]),
    // FEAT_LRCPC3
    ("rcpc3", Unstable(sym::aarch64_unstable_target_feature), &["rcpc2"]),
    // FEAT_RDM
    ("rdm", Stable, &["neon"]),
    (
        "reserve-x18",
        // Not implied by any CPU model or other feature; the compiler flag is a target modifier.
        InternalOnly { reason: "use `-Zfixed-x18` compiler flag instead", hard_error: false },
        &[],
    ),
    // FEAT_SB
    ("sb", Stable, &[]),
    // FEAT_SHA1 & FEAT_SHA256
    ("sha2", Stable, &["neon"]),
    // FEAT_SHA512 & FEAT_SHA3
    ("sha3", Stable, &["sha2"]),
    // FEAT_SM3 & FEAT_SM4
    ("sm4", Stable, &["neon"]),
    // FEAT_SME
    ("sme", Unstable(sym::aarch64_unstable_target_feature), &["bf16"]),
    // FEAT_SME_B16B16
    ("sme-b16b16", Unstable(sym::aarch64_unstable_target_feature), &["bf16", "sme2", "sve-b16b16"]),
    // FEAT_SME_F8F16
    ("sme-f8f16", Unstable(sym::aarch64_unstable_target_feature), &["sme-f8f32"]),
    // FEAT_SME_F8F32
    ("sme-f8f32", Unstable(sym::aarch64_unstable_target_feature), &["sme2", "fp8"]),
    // FEAT_SME_F16F16
    ("sme-f16f16", Unstable(sym::aarch64_unstable_target_feature), &["sme2"]),
    // FEAT_SME_F64F64
    ("sme-f64f64", Unstable(sym::aarch64_unstable_target_feature), &["sme"]),
    // FEAT_SME_FA64
    ("sme-fa64", Unstable(sym::aarch64_unstable_target_feature), &["sme", "sve2"]),
    // FEAT_SME_I16I64
    ("sme-i16i64", Unstable(sym::aarch64_unstable_target_feature), &["sme"]),
    // FEAT_SME_LUTv2
    ("sme-lutv2", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // FEAT_SME2
    ("sme2", Unstable(sym::aarch64_unstable_target_feature), &["sme"]),
    // FEAT_SME2p1
    ("sme2p1", Unstable(sym::aarch64_unstable_target_feature), &["sme2"]),
    // FEAT_SPE
    ("spe", Stable, &[]),
    // FEAT_SSBS & FEAT_SSBS2
    ("ssbs", Stable, &[]),
    // FEAT_SSVE_FP8FDOT2
    ("ssve-fp8dot2", Unstable(sym::aarch64_unstable_target_feature), &["ssve-fp8dot4"]),
    // FEAT_SSVE_FP8FDOT4
    ("ssve-fp8dot4", Unstable(sym::aarch64_unstable_target_feature), &["ssve-fp8fma"]),
    // FEAT_SSVE_FP8FMA
    ("ssve-fp8fma", Unstable(sym::aarch64_unstable_target_feature), &["sme2", "fp8"]),
    // FEAT_SVE
    // It was decided that SVE requires Neon: https://github.com/rust-lang/rust/pull/91608
    //
    // LLVM doesn't enable Neon for SVE. ARM indicates that they're separate, but probably always
    // exist together: https://developer.arm.com/documentation/102340/0100/New-features-in-SVE2
    //
    // "For backwards compatibility, Neon and VFP are required in the latest architectures."
    ("sve", Stable, &["neon", "fp16"]),
    // FEAT_SVE_B16B16 (SVE or SME Z-targeting instructions)
    ("sve-b16b16", Unstable(sym::aarch64_unstable_target_feature), &["bf16"]),
    // FEAT_SVE2
    ("sve2", Stable, &["sve"]),
    // FEAT_SVE_AES & FEAT_SVE_PMULL128
    ("sve2-aes", Stable, &["sve2", "aes"]),
    // FEAT_SVE2_BitPerm
    ("sve2-bitperm", Stable, &["sve2"]),
    // FEAT_SVE2_SHA3
    ("sve2-sha3", Stable, &["sve2", "sha3"]),
    // FEAT_SVE2_SM4
    ("sve2-sm4", Stable, &["sve2", "sm4"]),
    // FEAT_SVE2p1
    ("sve2p1", Unstable(sym::aarch64_unstable_target_feature), &["sve2"]),
    // FEAT_TME
    ("tme", Stable, &[]),
    (
        "v8.1a",
        Unstable(sym::aarch64_ver_target_feature),
        &["crc", "lse", "rdm", "pan", "lor", "vh"],
    ),
    ("v8.2a", Unstable(sym::aarch64_ver_target_feature), &["v8.1a", "ras", "dpb"]),
    (
        "v8.3a",
        Unstable(sym::aarch64_ver_target_feature),
        &["v8.2a", "rcpc", "paca", "pacg", "jsconv"],
    ),
    ("v8.4a", Unstable(sym::aarch64_ver_target_feature), &["v8.3a", "dotprod", "dit", "flagm"]),
    ("v8.5a", Unstable(sym::aarch64_ver_target_feature), &["v8.4a", "ssbs", "sb", "dpb2", "bti"]),
    ("v8.6a", Unstable(sym::aarch64_ver_target_feature), &["v8.5a", "bf16", "i8mm"]),
    ("v8.7a", Unstable(sym::aarch64_ver_target_feature), &["v8.6a", "wfxt"]),
    ("v8.8a", Unstable(sym::aarch64_ver_target_feature), &["v8.7a", "hbc", "mops"]),
    ("v8.9a", Unstable(sym::aarch64_ver_target_feature), &["v8.8a", "cssc"]),
    ("v9.1a", Unstable(sym::aarch64_ver_target_feature), &["v9a", "v8.6a"]),
    ("v9.2a", Unstable(sym::aarch64_ver_target_feature), &["v9.1a", "v8.7a"]),
    ("v9.3a", Unstable(sym::aarch64_ver_target_feature), &["v9.2a", "v8.8a"]),
    ("v9.4a", Unstable(sym::aarch64_ver_target_feature), &["v9.3a", "v8.9a"]),
    ("v9.5a", Unstable(sym::aarch64_ver_target_feature), &["v9.4a"]),
    ("v9a", Unstable(sym::aarch64_ver_target_feature), &["v8.5a"]),
    // FEAT_VHE
    ("vh", Stable, &[]),
    // FEAT_WFxT
    ("wfxt", Unstable(sym::aarch64_unstable_target_feature), &[]),
    // tidy-alphabetical-end
];

const X86_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("adx", Stable, &[]),
    ("aes", Stable, &["sse2"]),
    ("amx-avx512", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-bf16", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-complex", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-fp8", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-fp16", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-int8", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-movrs", Unstable(sym::x86_amx_intrinsics), &["amx-tile"]),
    ("amx-tile", Unstable(sym::x86_amx_intrinsics), &[]),
    ("apxf", Unstable(sym::apx_target_feature), &[]),
    ("avx", Stable, &["sse4.2"]),
    ("avx2", Stable, &["avx"]),
    (
        "avx10.1",
        Unstable(sym::avx10_target_feature),
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
    (
        "avx10.2",
        Unstable(sym::avx10_target_feature),
        &["avx10.1", "avxvnni", "avxvnniint8", "avxvnniint16"],
    ),
    ("avx512bf16", Stable, &["avx512bw"]),
    ("avx512bitalg", Stable, &["avx512bw"]),
    ("avx512bw", Stable, &["avx512f"]),
    ("avx512cd", Stable, &["avx512f"]),
    ("avx512dq", Stable, &["avx512f"]),
    ("avx512f", Stable, &["avx2", "fma", "f16c"]),
    ("avx512fp16", Stable, &["avx512bw"]),
    ("avx512ifma", Stable, &["avx512f"]),
    ("avx512vbmi", Stable, &["avx512bw"]),
    ("avx512vbmi2", Stable, &["avx512bw"]),
    ("avx512vl", Stable, &["avx512f"]),
    ("avx512vnni", Stable, &["avx512f"]),
    ("avx512vp2intersect", Stable, &["avx512f"]),
    ("avx512vpopcntdq", Stable, &["avx512f"]),
    ("avxifma", Stable, &["avx2"]),
    ("avxneconvert", Stable, &["avx2"]),
    ("avxvnni", Stable, &["avx2"]),
    ("avxvnniint8", Stable, &["avx2"]),
    ("avxvnniint16", Stable, &["avx2"]),
    ("bmi1", Stable, &[]),
    ("bmi2", Stable, &[]),
    ("clflushopt", Unstable(sym::clflushopt_target_feature), &[]),
    ("cmpxchg16b", Stable, &[]),
    ("ermsb", Unstable(sym::ermsb_target_feature), &[]),
    ("f16c", Stable, &["avx"]),
    ("fma", Stable, &["avx"]),
    ("fma4", Unstable(sym::fma4_target_feature), &["avx", "sse4a"]),
    ("fxsr", Stable, &[]),
    ("gfni", Stable, &["sse2"]),
    ("kl", Stable, &["sse2"]),
    ("lahfsahf", Unstable(sym::lahfsahf_target_feature), &[]),
    ("lzcnt", Stable, &[]),
    ("movbe", Stable, &[]),
    ("movdir64b", Unstable(sym::movdir64b_target_feature), &[]),
    ("movdiri", Unstable(sym::movdiri_target_feature), &[]),
    ("movrs", Unstable(sym::movrs_target_feature), &[]),
    ("pclmulqdq", Stable, &["sse2"]),
    ("popcnt", Stable, &[]),
    ("prfchw", Unstable(sym::prfchw_target_feature), &[]),
    ("rdrand", Stable, &[]),
    ("rdseed", Stable, &[]),
    (
        "retpoline-external-thunk",
        // Not implied by any CPU model or other feature; the compiler flag is a target modifier.
        Stability::InternalOnly {
            reason: "use `-Zretpoline-external-thunk` compiler flag instead",
            hard_error: false,
        },
        &[],
    ),
    (
        "retpoline-indirect-branches",
        // Not implied by any CPU model or other feature; the compiler flag is a target modifier.
        Stability::InternalOnly {
            reason: "use `-Zretpoline` compiler flag instead",
            hard_error: false,
        },
        &[],
    ),
    (
        "retpoline-indirect-calls",
        // Not implied by any CPU model or other feature; the compiler flag is a target modifier.
        Stability::InternalOnly {
            reason: "use `-Zretpoline` compiler flag instead",
            hard_error: false,
        },
        &[],
    ),
    ("rtm", Unstable(sym::rtm_target_feature), &[]),
    ("sha", Stable, &["sse2"]),
    ("sha512", Stable, &["avx2"]),
    ("sm3", Stable, &["avx"]),
    ("sm4", Stable, &["avx2"]),
    (
        "soft-float",
        // Pinned down by [`Target::abi_required_features`].
        Stability::InternalOnly { reason: "use a soft-float target instead", hard_error: false },
        &[],
    ),
    ("sse", Stable, &[]),
    ("sse2", Stable, &["sse"]),
    ("sse3", Stable, &["sse2"]),
    ("sse4.1", Stable, &["ssse3"]),
    ("sse4.2", Stable, &["sse4.1"]),
    ("sse4a", Stable, &["sse3"]),
    ("ssse3", Stable, &["sse3"]),
    ("tbm", Stable, &[]),
    ("vaes", Stable, &["avx2", "aes"]),
    ("vpclmulqdq", Stable, &["avx", "pclmulqdq"]),
    ("widekl", Stable, &["kl"]),
    ("x87", Unstable(sym::x87_target_feature), &[]),
    ("xop", Unstable(sym::xop_target_feature), &["fma4", "avx", "sse4a"]),
    ("xsave", Stable, &[]),
    ("xsavec", Stable, &["xsave"]),
    ("xsaveopt", Stable, &["xsave"]),
    ("xsaves", Stable, &["xsave"]),
    // tidy-alphabetical-end
];

const HEXAGON_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("audio", Unstable(sym::hexagon_target_feature), &[]),
    ("hvx", Unstable(sym::hexagon_target_feature), &[]),
    ("hvx-ieee-fp", Unstable(sym::hexagon_target_feature), &["hvx"]),
    ("hvx-length64b", Unstable(sym::hexagon_target_feature), &["hvx"]),
    ("hvx-length128b", Unstable(sym::hexagon_target_feature), &["hvx"]),
    ("hvx-qfloat", Unstable(sym::hexagon_target_feature), &["hvx"]),
    ("hvxv60", Unstable(sym::hexagon_target_feature), &["hvx"]),
    ("hvxv62", Unstable(sym::hexagon_target_feature), &["hvxv60"]),
    ("hvxv65", Unstable(sym::hexagon_target_feature), &["hvxv62"]),
    ("hvxv66", Unstable(sym::hexagon_target_feature), &["hvxv65", "zreg"]),
    ("hvxv67", Unstable(sym::hexagon_target_feature), &["hvxv66"]),
    ("hvxv68", Unstable(sym::hexagon_target_feature), &["hvxv67"]),
    ("hvxv69", Unstable(sym::hexagon_target_feature), &["hvxv68"]),
    ("hvxv71", Unstable(sym::hexagon_target_feature), &["hvxv69"]),
    ("hvxv73", Unstable(sym::hexagon_target_feature), &["hvxv71"]),
    ("hvxv75", Unstable(sym::hexagon_target_feature), &["hvxv73"]),
    ("hvxv79", Unstable(sym::hexagon_target_feature), &["hvxv75"]),
    ("v60", Unstable(sym::hexagon_target_feature), &[]),
    ("v62", Unstable(sym::hexagon_target_feature), &["v60"]),
    ("v65", Unstable(sym::hexagon_target_feature), &["v62"]),
    ("v66", Unstable(sym::hexagon_target_feature), &["v65"]),
    ("v67", Unstable(sym::hexagon_target_feature), &["v66"]),
    ("v68", Unstable(sym::hexagon_target_feature), &["v67"]),
    ("v69", Unstable(sym::hexagon_target_feature), &["v68"]),
    ("v71", Unstable(sym::hexagon_target_feature), &["v69"]),
    ("v73", Unstable(sym::hexagon_target_feature), &["v71"]),
    ("v75", Unstable(sym::hexagon_target_feature), &["v73"]),
    ("v79", Unstable(sym::hexagon_target_feature), &["v75"]),
    ("zreg", Unstable(sym::hexagon_target_feature), &[]),
    // tidy-alphabetical-end
];

const POWERPC_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // If you are thinking of adding "efpu2" here, please double-check that it really does not
    // affect the ABI.
    // tidy-alphabetical-start
    ("altivec", Unstable(sym::powerpc_target_feature), &[]),
    (
        "hard-float",
        // Pinned down by [`Target::abi_required_features`].
        InternalOnly { reason: "unsupported ABI-configuration feature", hard_error: false },
        &[],
    ),
    ("msync", Unstable(sym::powerpc_target_feature), &[]),
    ("partword-atomics", Unstable(sym::powerpc_target_feature), &[]),
    ("power8-altivec", Unstable(sym::powerpc_target_feature), &["altivec"]),
    ("power8-crypto", Unstable(sym::powerpc_target_feature), &["power8-altivec"]),
    ("power8-vector", Unstable(sym::powerpc_target_feature), &["vsx", "power8-altivec"]),
    ("power9-altivec", Unstable(sym::powerpc_target_feature), &["power8-altivec"]),
    ("power9-vector", Unstable(sym::powerpc_target_feature), &["power8-vector", "power9-altivec"]),
    ("power10-vector", Unstable(sym::powerpc_target_feature), &["power9-vector"]),
    ("quadword-atomics", Unstable(sym::powerpc_target_feature), &[]),
    (
        "spe",
        // Pinned down by [`Target::abi_required_features`].
        InternalOnly { reason: "unsupported ABI-configuration feature", hard_error: false },
        &[],
    ),
    ("vsx", Unstable(sym::powerpc_target_feature), &["altivec"]),
    // tidy-alphabetical-end
];

const MIPS_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("fp64", Unstable(sym::mips_target_feature), &[]),
    // FIXME(#150253): msa requires either fp64 or no hard float support at all: LLVM requires fp64
    // FIXME(#150253): msa requires revision 5 or greater (mips32r5/mips64r5 in LLVM)
    ("msa", Unstable(sym::mips_target_feature), &[]),
    ("virt", Unstable(sym::mips_target_feature), &[]),
    // tidy-alphabetical-end
];

const NVPTX_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("sm_70", Unstable(sym::nvptx_target_feature), &[]),
    ("sm_72", Unstable(sym::nvptx_target_feature), &["sm_70"]),
    ("sm_75", Unstable(sym::nvptx_target_feature), &["sm_72"]),
    ("sm_80", Unstable(sym::nvptx_target_feature), &["sm_75"]),
    ("sm_86", Unstable(sym::nvptx_target_feature), &["sm_80"]),
    ("sm_87", Unstable(sym::nvptx_target_feature), &["sm_86"]),
    ("sm_89", Unstable(sym::nvptx_target_feature), &["sm_87"]),
    ("sm_90", Unstable(sym::nvptx_target_feature), &["sm_89"]),
    ("sm_90a", Unstable(sym::nvptx_target_feature), &["sm_90"]),
    // tidy-alphabetical-end
    // tidy-alphabetical-start
    ("sm_100", Unstable(sym::nvptx_target_feature), &["sm_90"]),
    ("sm_100a", Unstable(sym::nvptx_target_feature), &["sm_100"]),
    ("sm_101", Unstable(sym::nvptx_target_feature), &["sm_100"]),
    ("sm_101a", Unstable(sym::nvptx_target_feature), &["sm_101"]),
    ("sm_120", Unstable(sym::nvptx_target_feature), &["sm_101"]),
    ("sm_120a", Unstable(sym::nvptx_target_feature), &["sm_120"]),
    // tidy-alphabetical-end
    // tidy-alphabetical-start
    ("ptx70", Unstable(sym::nvptx_target_feature), &[]),
    ("ptx71", Unstable(sym::nvptx_target_feature), &["ptx70"]),
    ("ptx72", Unstable(sym::nvptx_target_feature), &["ptx71"]),
    ("ptx73", Unstable(sym::nvptx_target_feature), &["ptx72"]),
    ("ptx74", Unstable(sym::nvptx_target_feature), &["ptx73"]),
    ("ptx75", Unstable(sym::nvptx_target_feature), &["ptx74"]),
    ("ptx76", Unstable(sym::nvptx_target_feature), &["ptx75"]),
    ("ptx77", Unstable(sym::nvptx_target_feature), &["ptx76"]),
    ("ptx78", Unstable(sym::nvptx_target_feature), &["ptx77"]),
    ("ptx80", Unstable(sym::nvptx_target_feature), &["ptx78"]),
    ("ptx81", Unstable(sym::nvptx_target_feature), &["ptx80"]),
    ("ptx82", Unstable(sym::nvptx_target_feature), &["ptx81"]),
    ("ptx83", Unstable(sym::nvptx_target_feature), &["ptx82"]),
    ("ptx84", Unstable(sym::nvptx_target_feature), &["ptx83"]),
    ("ptx85", Unstable(sym::nvptx_target_feature), &["ptx84"]),
    ("ptx86", Unstable(sym::nvptx_target_feature), &["ptx85"]),
    ("ptx87", Unstable(sym::nvptx_target_feature), &["ptx86"]),
    // tidy-alphabetical-end
];

const RISCV_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("a", Stable, &["zaamo", "zalrsc"]),
    ("b", Stable, &["zba", "zbb", "zbs"]),
    ("c", Stable, &["zca"]),
    ("d", Stable, &["f"]),
    ("e", Unstable(sym::riscv_target_feature), &[]), // negative feature! needs special care.
    ("f", Stable, &["zicsr"]),
    (
        "forced-atomics",
        // Not implied by any CPU model or other feature.
        Stability::InternalOnly {
            reason: "unsound because it changes the ABI of atomic operations",
            hard_error: false,
        },
        &[],
    ),
    // According to the RISC-V spec, the M ISA extension (integer multiplication/division) is supported only when the M bit
    // of the misa register is 1, while Zmmul means integer multiplication is always supported.
    //
    // The Rust/LLVM "m" target feature means something slightly different: with "m" enabled, it is assumed that integer
    // multiplication and division will always be available (M bit will be 1 in misa), so "m" implies "zmmul".
    //
    // See discussion in the PR adding Zmmul: https://github.com/rust-lang/rust/pull/162552
    ("m", Stable, &["zmmul"]),
    ("relax", Unstable(sym::riscv_target_feature), &[]),
    (
        "rva23u64",
        Unstable(sym::riscv_target_feature),
        &[
            "m",
            "a",
            "f",
            "d",
            "c",
            "b",
            "v",
            "zicsr",
            "zicntr",
            "zihpm",
            "ziccif",
            "ziccrse",
            "ziccamoa",
            "zicclsm",
            "zic64b",
            "za64rs",
            "zihintpause",
            "zba",
            "zbb",
            "zbs",
            "zicbom",
            "zicbop",
            "zicboz",
            "zfhmin",
            "zkt",
            "zvfhmin",
            "zvbb",
            "zvkt",
            "zihintntl",
            "zicond",
            "zimop",
            "zcmop",
            "zcb",
            "zfa",
            "zawrs",
            "supm",
        ],
    ),
    ("supm", Unstable(sym::riscv_target_feature), &[]),
    ("unaligned-scalar-mem", Unstable(sym::riscv_target_feature), &[]),
    ("unaligned-vector-mem", Unstable(sym::riscv_target_feature), &[]),
    ("v", Unstable(sym::riscv_target_feature), &["zvl128b", "zve64d"]),
    ("za64rs", Stable, &["za128rs"]), // Za64rs ⊃ Za128rs
    ("za128rs", Stable, &[]),
    ("zaamo", Stable, &[]),
    ("zabha", Stable, &["zaamo"]),
    ("zacas", Stable, &["zaamo"]),
    ("zalrsc", Stable, &[]),
    ("zama16b", Stable, &[]),
    ("zawrs", Stable, &[]),
    ("zba", Stable, &[]),
    ("zbb", Stable, &[]),
    ("zbc", Stable, &["zbkc"]), // Zbc ⊃ Zbkc
    ("zbkb", Stable, &[]),
    ("zbkc", Stable, &[]),
    ("zbkx", Stable, &[]),
    ("zbs", Stable, &[]),
    ("zca", Stable, &[]),
    ("zcb", Stable, &["zca"]),
    ("zcmop", Stable, &["zca"]),
    ("zdinx", Unstable(sym::riscv_target_feature), &["zfinx"]),
    ("zfa", Unstable(sym::riscv_target_feature), &["f"]),
    ("zfbfmin", Unstable(sym::riscv_target_feature), &["f"]), // and a subset of Zfhmin
    ("zfh", Unstable(sym::riscv_target_feature), &["zfhmin"]),
    ("zfhmin", Unstable(sym::riscv_target_feature), &["f"]),
    ("zfinx", Unstable(sym::riscv_target_feature), &["zicsr"]),
    ("zhinx", Unstable(sym::riscv_target_feature), &["zhinxmin"]),
    ("zhinxmin", Unstable(sym::riscv_target_feature), &["zfinx"]),
    ("zic64b", Stable, &[]),
    ("zicbom", Stable, &[]),
    ("zicbop", Stable, &[]),
    ("zicboz", Stable, &[]),
    ("ziccamoa", Stable, &[]),
    ("ziccif", Stable, &[]),
    ("zicclsm", Stable, &[]),
    ("ziccrse", Stable, &[]),
    ("zicntr", Stable, &["zicsr"]),
    ("zicond", Stable, &[]),
    ("zicsr", Stable, &[]),
    ("zifencei", Stable, &[]),
    ("zihintntl", Stable, &[]),
    ("zihintpause", Stable, &[]),
    ("zihpm", Stable, &["zicsr"]),
    ("zimop", Stable, &[]),
    ("zk", Stable, &["zkn", "zkr", "zkt"]),
    ("zkn", Stable, &["zbkb", "zbkc", "zbkx", "zkne", "zknd", "zknh"]),
    ("zknd", Stable, &["zkne_or_zknd"]),
    ("zkne", Stable, &["zkne_or_zknd"]),
    ("zkne_or_zknd", Unstable(sym::riscv_target_feature), &[]), // Not an extension
    ("zknh", Stable, &[]),
    ("zkr", Stable, &[]),
    ("zks", Stable, &["zbkb", "zbkc", "zbkx", "zksed", "zksh"]),
    ("zksed", Stable, &[]),
    ("zksh", Stable, &[]),
    ("zkt", Stable, &[]),
    ("zmmul", Unstable(sym::riscv_target_feature), &[]),
    ("ztso", Stable, &[]),
    ("zvbb", Unstable(sym::riscv_target_feature), &["zvkb"]), // Zvbb ⊃ Zvkb
    ("zvbc", Unstable(sym::riscv_target_feature), &["zve64x"]),
    ("zve32f", Unstable(sym::riscv_target_feature), &["zve32x", "f"]),
    ("zve32x", Unstable(sym::riscv_target_feature), &["zvl32b", "zicsr"]),
    ("zve64d", Unstable(sym::riscv_target_feature), &["zve64f", "d"]),
    ("zve64f", Unstable(sym::riscv_target_feature), &["zve32f", "zve64x"]),
    ("zve64x", Unstable(sym::riscv_target_feature), &["zve32x", "zvl64b"]),
    ("zvfbfmin", Unstable(sym::riscv_target_feature), &["zve32f"]),
    ("zvfbfwma", Unstable(sym::riscv_target_feature), &["zfbfmin", "zvfbfmin"]),
    ("zvfh", Unstable(sym::riscv_target_feature), &["zvfhmin", "zve32f", "zfhmin"]), // Zvfh ⊃ Zvfhmin
    ("zvfhmin", Unstable(sym::riscv_target_feature), &["zve32f"]),
    ("zvkb", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvkg", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvkn", Unstable(sym::riscv_target_feature), &["zvkned", "zvknhb", "zvkb", "zvkt"]),
    ("zvknc", Unstable(sym::riscv_target_feature), &["zvkn", "zvbc"]),
    ("zvkned", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvkng", Unstable(sym::riscv_target_feature), &["zvkn", "zvkg"]),
    ("zvknha", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvknhb", Unstable(sym::riscv_target_feature), &["zvknha", "zve64x"]), // Zvknhb ⊃ Zvknha
    ("zvks", Unstable(sym::riscv_target_feature), &["zvksed", "zvksh", "zvkb", "zvkt"]),
    ("zvksc", Unstable(sym::riscv_target_feature), &["zvks", "zvbc"]),
    ("zvksed", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvksg", Unstable(sym::riscv_target_feature), &["zvks", "zvkg"]),
    ("zvksh", Unstable(sym::riscv_target_feature), &["zve32x"]),
    ("zvkt", Unstable(sym::riscv_target_feature), &[]),
    ("zvl32b", Unstable(sym::riscv_target_feature), &[]),
    ("zvl64b", Unstable(sym::riscv_target_feature), &["zvl32b"]),
    ("zvl128b", Unstable(sym::riscv_target_feature), &["zvl64b"]),
    ("zvl256b", Unstable(sym::riscv_target_feature), &["zvl128b"]),
    ("zvl512b", Unstable(sym::riscv_target_feature), &["zvl256b"]),
    ("zvl1024b", Unstable(sym::riscv_target_feature), &["zvl512b"]),
    ("zvl2048b", Unstable(sym::riscv_target_feature), &["zvl1024b"]),
    ("zvl4096b", Unstable(sym::riscv_target_feature), &["zvl2048b"]),
    ("zvl8192b", Unstable(sym::riscv_target_feature), &["zvl4096b"]),
    ("zvl16384b", Unstable(sym::riscv_target_feature), &["zvl8192b"]),
    ("zvl32768b", Unstable(sym::riscv_target_feature), &["zvl16384b"]),
    ("zvl65536b", Unstable(sym::riscv_target_feature), &["zvl32768b"]),
    // tidy-alphabetical-end
];

const WASM_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("atomics", Unstable(sym::wasm_target_feature), &[]),
    ("bulk-memory", Stable, &[]),
    ("exception-handling", Unstable(sym::wasm_target_feature), &[]),
    ("extended-const", Stable, &[]),
    ("gc", Unstable(sym::wasm_target_feature), &["reference-types"]),
    ("multivalue", Stable, &[]),
    ("mutable-globals", Stable, &[]),
    ("nontrapping-fptoint", Stable, &[]),
    ("reference-types", Stable, &[]),
    ("relaxed-simd", Stable, &["simd128"]),
    ("sign-ext", Stable, &[]),
    ("simd128", Stable, &[]),
    ("tail-call", Stable, &[]),
    ("wide-arithmetic", Unstable(sym::wasm_target_feature), &[]),
    // tidy-alphabetical-end
];

const BPF_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    ("alu32", Unstable(sym::bpf_target_feature), &[]),
    ("allows-misaligned-mem-access", Unstable(sym::bpf_target_feature), &[]),
];

const CSKY_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("2e3", Unstable(sym::csky_target_feature), &["e2"]),
    ("3e3r1", Unstable(sym::csky_target_feature), &[]),
    ("3e3r2", Unstable(sym::csky_target_feature), &["3e3r1", "doloop"]),
    ("3e3r3", Unstable(sym::csky_target_feature), &["doloop"]),
    ("3e7", Unstable(sym::csky_target_feature), &["2e3"]),
    ("7e10", Unstable(sym::csky_target_feature), &["3e7"]),
    ("10e60", Unstable(sym::csky_target_feature), &["7e10"]),
    ("cache", Unstable(sym::csky_target_feature), &[]),
    ("doloop", Unstable(sym::csky_target_feature), &[]),
    ("dsp1e2", Unstable(sym::csky_target_feature), &[]),
    ("dspe60", Unstable(sym::csky_target_feature), &[]),
    ("e1", Unstable(sym::csky_target_feature), &["elrw"]),
    ("e2", Unstable(sym::csky_target_feature), &["e2"]),
    ("edsp", Unstable(sym::csky_target_feature), &[]),
    ("elrw", Unstable(sym::csky_target_feature), &[]),
    ("float1e2", Unstable(sym::csky_target_feature), &[]),
    ("float1e3", Unstable(sym::csky_target_feature), &[]),
    ("float3e4", Unstable(sym::csky_target_feature), &[]),
    ("float7e60", Unstable(sym::csky_target_feature), &[]),
    ("floate1", Unstable(sym::csky_target_feature), &[]),
    ("hard-tp", Unstable(sym::csky_target_feature), &[]),
    ("high-registers", Unstable(sym::csky_target_feature), &[]),
    ("hwdiv", Unstable(sym::csky_target_feature), &[]),
    ("mp", Unstable(sym::csky_target_feature), &["2e3"]),
    ("mp1e2", Unstable(sym::csky_target_feature), &["3e7"]),
    ("nvic", Unstable(sym::csky_target_feature), &[]),
    ("trust", Unstable(sym::csky_target_feature), &[]),
    ("vdsp2e60f", Unstable(sym::csky_target_feature), &[]),
    ("vdspv1", Unstable(sym::csky_target_feature), &[]),
    ("vdspv2", Unstable(sym::csky_target_feature), &[]),
    // tidy-alphabetical-end
    //fpu
    // tidy-alphabetical-start
    ("fdivdu", Unstable(sym::csky_target_feature), &[]),
    ("fpuv2_df", Unstable(sym::csky_target_feature), &[]),
    ("fpuv2_sf", Unstable(sym::csky_target_feature), &[]),
    ("fpuv3_df", Unstable(sym::csky_target_feature), &[]),
    ("fpuv3_hf", Unstable(sym::csky_target_feature), &[]),
    ("fpuv3_hi", Unstable(sym::csky_target_feature), &[]),
    ("fpuv3_sf", Unstable(sym::csky_target_feature), &[]),
    ("hard-float", Unstable(sym::csky_target_feature), &[]),
    ("hard-float-abi", Unstable(sym::csky_target_feature), &[]),
    // tidy-alphabetical-end
];

const LOONGARCH_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("32s", Unstable(sym::loongarch_target_feature), &[]),
    ("d", Stable, &["f"]),
    ("div32", Stable, &[]),
    ("f", Stable, &[]),
    ("frecipe", Stable, &[]),
    ("lam-bh", Stable, &[]),
    ("lamcas", Stable, &[]),
    ("lasx", Stable, &["lsx"]),
    ("lbt", Stable, &[]),
    ("ld-seq-sa", Stable, &[]),
    ("lsx", Stable, &["d"]),
    ("lvz", Stable, &[]),
    ("relax", Unstable(sym::loongarch_target_feature), &[]),
    ("scq", Stable, &[]),
    ("ual", Unstable(sym::loongarch_target_feature), &[]),
    // tidy-alphabetical-end
];

#[rustfmt::skip]
const IBMZ_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    // For "backchain", https://github.com/rust-lang/rust/issues/142412 is a stabilization blocker
    ("backchain", Unstable(sym::s390x_target_feature), &[]),
    ("concurrent-functions", Unstable(sym::s390x_target_feature), &[]),
    ("deflate-conversion", Unstable(sym::s390x_target_feature), &[]),
    ("enhanced-sort", Unstable(sym::s390x_target_feature), &[]),
    ("guarded-storage", Unstable(sym::s390x_target_feature), &[]),
    ("high-word", Unstable(sym::s390x_target_feature), &[]),
    // LLVM does not define message-security-assist-extension versions 1, 2, 6, 10 and 11.
    ("message-security-assist-extension3", Unstable(sym::s390x_target_feature), &[]),
    ("message-security-assist-extension4", Unstable(sym::s390x_target_feature), &[]),
    ("message-security-assist-extension5", Unstable(sym::s390x_target_feature), &[]),
    ("message-security-assist-extension8", Unstable(sym::s390x_target_feature), &["message-security-assist-extension3"]),
    ("message-security-assist-extension9", Unstable(sym::s390x_target_feature), &["message-security-assist-extension3", "message-security-assist-extension4"]),
    ("message-security-assist-extension12", Unstable(sym::s390x_target_feature), &[]),
    ("miscellaneous-extensions-2", Stable, &[]),
    ("miscellaneous-extensions-3", Stable, &[]),
    ("miscellaneous-extensions-4", Stable, &[]),
    ("nnp-assist", Stable, &["vector"]),
    // Pinned down by [`Target::abi_required_features`].
    ("soft-float", InternalOnly { reason: "unsupported ABI-configuration feature", hard_error: false }, &[]),
    ("transactional-execution", Unstable(sym::s390x_target_feature), &[]),
    ("vector", Stable, &[]),
    ("vector-enhancements-1", Stable, &["vector"]),
    ("vector-enhancements-2", Stable, &["vector-enhancements-1"]),
    ("vector-enhancements-3", Stable, &["vector-enhancements-2"]),
    ("vector-packed-decimal", Stable, &["vector"]),
    ("vector-packed-decimal-enhancement", Stable, &["vector-packed-decimal"]),
    ("vector-packed-decimal-enhancement-2", Stable, &["vector-packed-decimal-enhancement"]),
    ("vector-packed-decimal-enhancement-3", Stable, &["vector-packed-decimal-enhancement-2"]),
    // tidy-alphabetical-end
];

const SPARC_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("leoncasa", Unstable(sym::sparc_target_feature), &[]),
    (
        "soft-float",
        InternalOnly { reason: "unsupported ABI-configuration feature", hard_error: false },
        &[],
    ),
    ("v8plus", Unstable(sym::sparc_target_feature), &[]),
    // FIXME: It's unclear what this feature means when `v8plus` is disabled on 32-bit SPARC. See
    // the discussion around https://github.com/rust-lang/rust/pull/160949#discussion_r3806194355.
    ("v9", Unstable(sym::sparc_target_feature), &[]),
    // tidy-alphabetical-end
];

const M68K_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("isa-68000", Unstable(sym::m68k_target_feature), &[]),
    ("isa-68010", Unstable(sym::m68k_target_feature), &["isa-68000"]),
    ("isa-68020", Unstable(sym::m68k_target_feature), &["isa-68010"]),
    ("isa-68030", Unstable(sym::m68k_target_feature), &["isa-68020"]),
    ("isa-68040", Unstable(sym::m68k_target_feature), &["isa-68030", "isa-68882"]),
    ("isa-68060", Unstable(sym::m68k_target_feature), &["isa-68040"]),
    // FPU
    ("isa-68881", Unstable(sym::m68k_target_feature), &[]),
    ("isa-68882", Unstable(sym::m68k_target_feature), &["isa-68881"]),
    // tidy-alphabetical-end
];

const AVR_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    // tidy-alphabetical-start
    ("addsubiw", Unstable(sym::avr_target_feature), &[]),
    ("break", Unstable(sym::avr_target_feature), &[]),
    ("eijmpcall", Unstable(sym::avr_target_feature), &[]),
    ("elpm", Unstable(sym::avr_target_feature), &[]),
    ("elpmx", Unstable(sym::avr_target_feature), &[]),
    ("ijmpcall", Unstable(sym::avr_target_feature), &[]),
    ("jmpcall", Unstable(sym::avr_target_feature), &[]),
    ("lowbytefirst", Unstable(sym::avr_target_feature), &[]),
    ("lpm", Unstable(sym::avr_target_feature), &[]),
    ("lpmx", Unstable(sym::avr_target_feature), &[]),
    ("movw", Unstable(sym::avr_target_feature), &[]),
    ("mul", Unstable(sym::avr_target_feature), &[]),
    ("rmw", Unstable(sym::avr_target_feature), &[]),
    ("spm", Unstable(sym::avr_target_feature), &[]),
    ("spmx", Unstable(sym::avr_target_feature), &[]),
    (
        "sram",
        // Pinned down by [`Target::abi_required_features`].
        InternalOnly { reason: "devices that have no SRAM are unsupported", hard_error: false },
        &[],
    ),
    ("tinyencoding", Unstable(sym::avr_target_feature), &[]),
    // tidy-alphabetical-end
];

const XTENSA_FEATURES: &[(&str, Stability, ImpliedFeatures)] = &[
    ("bool", Unstable(sym::xtensa_target_feature), &[]),
    ("fp", Unstable(sym::xtensa_target_feature), &["bool", "coprocessor"]),
    ("coprocessor", Unstable(sym::xtensa_target_feature), &[]),
    ("highpriinterrupts", Unstable(sym::xtensa_target_feature), &["interrupt"]),
    ("interrupt", Unstable(sym::xtensa_target_feature), &["exception"]),
    (
        "windowed",
        // Pinned down by [`Target::abi_required_features`].
        InternalOnly {
            reason: "windowed changes the Xtensa calling convention",
            hard_error: false,
        },
        &["exception"],
    ),
    ("loop", Unstable(sym::xtensa_target_feature), &[]),
    ("sext", Unstable(sym::xtensa_target_feature), &[]),
    ("nsa", Unstable(sym::xtensa_target_feature), &[]),
    ("mul32", Unstable(sym::xtensa_target_feature), &[]),
    ("mul32high", Unstable(sym::xtensa_target_feature), &["mul32"]),
    ("div32", Unstable(sym::xtensa_target_feature), &[]),
    ("mac16", Unstable(sym::xtensa_target_feature), &[]),
    ("s32c1i", Unstable(sym::xtensa_target_feature), &[]),
    ("threadptr", Unstable(sym::xtensa_target_feature), &[]),
    ("extendedl32r", Unstable(sym::xtensa_target_feature), &[]),
    ("debug", Unstable(sym::xtensa_target_feature), &["exception"]),
    ("exception", Unstable(sym::xtensa_target_feature), &[]),
    ("rvector", Unstable(sym::xtensa_target_feature), &["exception"]),
    ("prid", Unstable(sym::xtensa_target_feature), &[]),
    ("regprotect", Unstable(sym::xtensa_target_feature), &[]),
    ("miscsr", Unstable(sym::xtensa_target_feature), &[]),
];

const TARGET_FEATURE_IMPLICATIONS_RAW: &[&[(&str, Stability, ImpliedFeatures)]] = &[
    ARM_FEATURES,
    AARCH64_FEATURES,
    X86_FEATURES,
    HEXAGON_FEATURES,
    POWERPC_FEATURES,
    MIPS_FEATURES,
    NVPTX_FEATURES,
    RISCV_FEATURES,
    WASM_FEATURES,
    BPF_FEATURES,
    XTENSA_FEATURES,
    CSKY_FEATURES,
    LOONGARCH_FEATURES,
    IBMZ_FEATURES,
    SPARC_FEATURES,
    M68K_FEATURES,
    AVR_FEATURES,
];
// spellchecker:on
