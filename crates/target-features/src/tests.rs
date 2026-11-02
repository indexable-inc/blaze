use std::collections::{BTreeSet, HashMap, HashSet};

use super::*;

/// Every arch, in the order of the tables, then the one without a table.
const ARCH_LIST: [TargetArch; ARCHES + 1] = [
    TargetArch::Arm,
    TargetArch::AArch64,
    TargetArch::X86,
    TargetArch::Hexagon,
    TargetArch::PowerPC,
    TargetArch::Mips,
    TargetArch::Nvptx,
    TargetArch::RiscV,
    TargetArch::Wasm,
    TargetArch::Bpf,
    TargetArch::Xtensa,
    TargetArch::Csky,
    TargetArch::LoongArch,
    TargetArch::IbmZ,
    TargetArch::Sparc,
    TargetArch::M68k,
    TargetArch::Avr,
    TargetArch::Other,
];

/// rustc's `implied_target_features`' map of `arch`'s table: each name, what its (last) line
/// says it implies.
fn rustc_map(arch: TargetArch) -> HashMap<&'static str, &'static [&'static str]> {
    arch.rustc_table().collect()
}

/// rustc's `implied_target_features` of each feature of `enabled`, added to it.
fn rustc_expand(map: &HashMap<&'static str, &'static [&'static str]>, enabled: &mut HashSet<&'static str>) {
    let mut queue = enabled.iter().copied().collect::<Vec<_>>();
    while let Some(feature) = queue.pop() {
        if let Some(implications) = map.get(feature) {
            for &implication in implications.iter() {
                if enabled.insert(implication) {
                    queue.push(implication);
                }
            }
        }
    }
}

#[test]
fn archs_are_the_tables_in_order() {
    for (at, arch) in ARCH_LIST.iter().enumerate() {
        assert_eq!(*arch as usize, at, "{arch:?}");
    }
    let cases = [
        ("arm", TargetArch::Arm),
        ("aarch64", TargetArch::AArch64),
        ("arm64ec", TargetArch::AArch64),
        ("x86", TargetArch::X86),
        ("x86_64", TargetArch::X86),
        ("hexagon", TargetArch::Hexagon),
        ("mips64r6", TargetArch::Mips),
        ("nvptx64", TargetArch::Nvptx),
        ("powerpc64", TargetArch::PowerPC),
        ("riscv32", TargetArch::RiscV),
        ("riscv64", TargetArch::RiscV),
        ("wasm32", TargetArch::Wasm),
        ("bpf", TargetArch::Bpf),
        ("csky", TargetArch::Csky),
        ("loongarch64", TargetArch::LoongArch),
        ("s390x", TargetArch::IbmZ),
        ("sparc64", TargetArch::Sparc),
        ("m68k", TargetArch::M68k),
        ("avr", TargetArch::Avr),
        ("xtensa", TargetArch::Xtensa),
        ("amdgpu", TargetArch::Other),
        ("msp430", TargetArch::Other),
        ("", TargetArch::Other),
    ];
    for (name, arch) in cases {
        assert_eq!(TargetArch::from_target_arch(name), arch, "{name}");
    }
    assert_eq!(TargetArch::Other.rustc_table().count(), 0);
}

#[test]
fn names_are_every_table_name_once() {
    assert!(NAMES.windows(2).all(|it| it[0].as_bytes() < it[1].as_bytes()), "sorted, once each");
    let mut tables = BTreeSet::new();
    for table in TARGET_FEATURE_IMPLICATIONS_RAW {
        for &(name, _, implied) in table.iter() {
            tables.insert(name);
            tables.extend(implied.iter().copied());
        }
    }
    assert_eq!(NAMES.iter().copied().collect::<BTreeSet<_>>(), tables);
    assert_eq!(NAMES.len(), tables.len());
    assert_eq!(TargetFeature::COUNT, NAMES.len());
    assert!(WORDS * 64 >= COUNT && (WORDS - 1) * 64 < COUNT);
    assert!(TargetFeature::all().map(TargetFeature::name).eq(NAMES.iter().copied()));
}

#[test]
fn perfect_hash_finds_each_name_and_nothing_else() {
    for (at, &name) in NAMES.iter().enumerate() {
        let feature = TargetFeature::from_name(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(feature.index(), at, "{name}");
        assert_eq!(feature.name(), name);
        assert_eq!(TargetFeature::named(name), feature);
    }
    let mut others: Vec<String> = vec![
        String::new(),
        " ".to_owned(),
        "avx 2".to_owned(),
        "AVX2".to_owned(),
        "sse4".to_owned(),
        "v8.10a".to_owned(),
        "crt-static".to_owned(),
        "64bit".to_owned(),
    ];
    for name in NAMES {
        others.push(format!("{name}x"));
        others.push(format!("+{name}"));
        others.push(format!(" {name}"));
        others.push(name[..name.len() - 1].to_owned());
        others.push(name.to_uppercase());
    }
    for other in &others {
        let linear = NAMES.iter().position(|it| *it == other.as_str());
        assert_eq!(TargetFeature::from_name(other).map(TargetFeature::index), linear, "{other:?}");
    }
}

#[test]
fn implications_are_rustcs_of_each_arch() {
    for arch in ARCH_LIST {
        let map = rustc_map(arch);
        for &name in NAMES {
            let mut old = HashSet::from([name]);
            rustc_expand(&map, &mut old);
            let old: BTreeSet<&str> = old.into_iter().collect();
            let feature = TargetFeature::named(name);
            let new: BTreeSet<&str> = feature.implied_in(arch).names().collect();
            assert_eq!(new, old, "{name} on {arch:?}");
            assert!(feature.implied_in(arch).contains(feature));
        }
    }
    // Spot checks against rustc's tables.
    let avx512f = TargetFeature::AVX512F.implied_in(TargetArch::X86);
    for name in ["avx2", "avx", "sse4.2", "sse", "fma", "f16c"] {
        assert!(avx512f.contains(TargetFeature::named(name)), "{name}");
    }
    assert!(!avx512f.contains(TargetFeature::AVX512VL));
    let wasm = TargetFeature::RELAXED_SIMD.implied_in(TargetArch::Wasm);
    assert!(wasm.contains(TargetFeature::SIMD128));
    assert!(TargetFeature::named("e2").implied_in(TargetArch::Csky).names().eq(["e2"]));
    // A name of two tables implies what its target's table says, not both.
    let aes = TargetFeature::named("aes");
    assert!(aes.implied_in(TargetArch::X86).names().eq(["aes", "sse", "sse2"]));
    assert!(aes.implied_in(TargetArch::AArch64).names().eq(["aes", "neon"]));
    let arm = aes.implied_in(TargetArch::Arm);
    assert!(arm.contains(TargetFeature::named("vfp3")));
    assert!(!arm.contains(TargetFeature::named("sse2")));
    // A feature of another arch's table implies nothing.
    assert!(TargetFeature::AVX512F.implied_in(TargetArch::AArch64).names().eq(["avx512f"]));
    assert!(TargetFeature::AVX512F.implied_in(TargetArch::Other).names().eq(["avx512f"]));
}

#[test]
fn constants_name_their_features() {
    let named = [
        (TargetFeature::AVX512BF16, "avx512bf16"),
        (TargetFeature::AVX512F, "avx512f"),
        (TargetFeature::AVX512FP16, "avx512fp16"),
        (TargetFeature::AVX512VL, "avx512vl"),
        (TargetFeature::AVXNECONVERT, "avxneconvert"),
        (TargetFeature::AVXVNNI, "avxvnni"),
        (TargetFeature::F16C, "f16c"),
        (TargetFeature::FMA, "fma"),
        (TargetFeature::FMA4, "fma4"),
        (TargetFeature::FP16, "fp16"),
        (TargetFeature::OUTLINE_ATOMICS, "outline-atomics"),
        (TargetFeature::RELAXED_SIMD, "relaxed-simd"),
        (TargetFeature::SIMD128, "simd128"),
    ];
    for (feature, name) in named {
        assert_eq!(feature.name(), name);
        assert_eq!(TargetFeature::from_name(name), Some(feature));
    }
}

#[test]
fn sets_iterate_in_name_order() {
    let set =
        TargetFeatureSet::of(&[TargetFeature::FMA, TargetFeature::AVX512F, TargetFeature::F16C]);
    assert!(set.names().eq(["avx512f", "f16c", "fma"]));
    assert_eq!(format!("{set:?}"), r#"{"avx512f", "f16c", "fma"}"#);
    let last = TargetFeature::named(NAMES[COUNT - 1]);
    let all: TargetFeatureSet = TargetFeature::all().collect();
    assert!(all.iter().map(TargetFeature::index).eq(0..COUNT));
    assert!(all.contains(last) && all.is_superset(&set) && !set.is_superset(&all));
    assert!(TargetFeatureSet::EMPTY.is_empty() && !set.is_empty());
    let mut grown = TargetFeatureSet::EMPTY;
    grown.insert(last);
    assert_eq!(grown, TargetFeatureSet::EMPTY.with(last));
    assert!(grown.iter().eq([last]));
}
