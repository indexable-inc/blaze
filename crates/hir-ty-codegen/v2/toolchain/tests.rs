use super::*;

fn of(target: &str, flags: &[&str]) -> Result<Toolchain, String> {
    runs(target, flags, Runs::Elsewhere)
}

fn here(flags: &[&str]) -> Result<Toolchain, String> {
    runs(Target::host(), flags, Runs::Here)
}

fn runs(target: &str, flags: &[&str], runs: Runs) -> Result<Toolchain, String> {
    let flags: Vec<String> = flags.iter().map(|it| (*it).to_owned()).collect();
    toolchain(target, &flags, runs)
}

/// Whether blaze emits code for the machine the tests run on.
fn host_is_a_target() -> bool {
    Target::parse(Target::host()).is_ok()
}

const LINUX: &str = "x86_64-unknown-linux-gnu";

#[test]
fn no_flags_is_the_targets_baseline() {
    let it = of(LINUX, &[]).unwrap();
    assert_eq!(it.target, LINUX);
    assert_eq!(it.arch, Arch::X86_64);
    assert_eq!(it.cpu, "x86-64");
    assert!(it.features.is_empty(), "{:?}", it.features);
    assert!(!it.frame_pointers);
    assert_eq!(it.relocation, Relocation::Pic);
    assert_eq!(it.cranelift, CRANELIFT);
    assert_eq!(it.id, of(LINUX, &[]).unwrap().id, "one identity for one blaze");
}

/// The code under a code hash is what this blaze makes of it: a store kept across blaze builds
/// (one `BLAZE_HOME` the self-build reuses) held another build's template under the key of
/// one this build made, and the put of this build's was refused as other bytes.
#[test]
fn another_blaze_build_is_another_toolchain() {
    assert!(NIGHTLY.starts_with("nightly-") && !NIGHTLY.contains(char::is_whitespace), "{NIGHTLY:?}");
    let this = build_of(&std::env::current_exe().unwrap()).unwrap();
    assert_eq!(build().unwrap(), this);
    assert_eq!(of(LINUX, &[]).unwrap().id, id_of(&this));
    let mut other = this;
    other.0[0] ^= 1;
    assert_ne!(id_of(&other), id_of(&this));
}

#[test]
fn a_feature_turns_on_what_it_implies_and_off_what_implies_it() {
    let on = of(LINUX, &["-C", "target-feature=+avx2"]).unwrap();
    assert_eq!(on.features, ["+avx", "+avx2", "+sse3", "+sse4.1", "+sse4.2", "+ssse3"]);
    let off = of(LINUX, &["-Ctarget-feature=+avx2", "-Ctarget-feature=-avx"]).unwrap();
    assert_eq!(off.features, ["+sse3", "+sse4.1", "+sse4.2", "+ssse3"]);
    let listed = of(LINUX, &["--codegen", "target-feature=+fma,+bmi2"]).unwrap();
    assert!(listed.features.contains(&"+fma".to_owned()) && listed.features.contains(&"+bmi2".to_owned()));
}

#[test]
fn a_microarchitecture_level_is_its_features() {
    let it = of(LINUX, &["-C", "target-cpu=x86-64-v3"]).unwrap();
    assert_eq!(it.cpu, "x86-64-v3");
    for feature in ["+avx2", "+bmi1", "+fma", "+movbe", "+popcnt", "+sse4.2"] {
        assert!(it.features.contains(&feature.to_owned()), "{feature}: {:?}", it.features);
    }
    assert!(!it.features.iter().any(|it| it.starts_with("+avx512")));
    assert!(it.features.is_sorted());
}

#[test]
fn flags_of_the_link_and_of_reports_are_not_the_codes() {
    let flags = [
        "-C", "link-arg=-fuse-ld=lld", "--cfg", "tokio_unstable", "-Dwarnings", "-Ccodegen-units=1",
        "-C", "lto=off", "-Ctarget-feature=+crt-static", "-L", "-Cnot-a-flag", "-Zshare-generics",
    ];
    assert_eq!(of(LINUX, &flags).unwrap(), of(LINUX, &[]).unwrap());
}

#[test]
fn a_flag_that_changes_code_and_is_not_read_is_an_error_naming_it() {
    for (flags, named) in [
        (&["-C", "opt-level=3"][..], "opt-level"),
        (&["-O"][..], "-O"),
        (&["-g"][..], "-g"),
        (&["-Cpanic=abort"][..], "panic"),
        (&["-C", "code-model=large"][..], "code-model"),
        (&["-C", "lto=fat"][..], "lto"),
        (&["-C", "target-cpu=skylake"][..], "skylake"),
        (&["-C", "target-feature=avx2"][..], "avx2"),
        (&["--remap-path-prefix=/a=/b"][..], "remap-path-prefix"),
        (&["-C", "relocation-model=ropi"][..], "ropi"),
        (&["-C", "passes=gvn"][..], "gvn"),
        (&["-C", "llvm-args=-x86-asm-syntax=intel"][..], "-x86-asm-syntax"),
        (&["-C"][..], "-C"),
    ] {
        let error = of(LINUX, flags).unwrap_err();
        assert!(error.contains(named), "{flags:?}: {error}");
    }
    assert!(of(LINUX, &["-C", "passes=sancov-module", "-C", "llvm-args=-sanitizer-coverage-level=3"]).is_ok());
}

#[test]
fn frame_pointers_are_the_flags_or_the_targets() {
    assert!(of(LINUX, &["--codegen", "force_frame_pointers=yes"]).unwrap().frame_pointers);
    assert!(of(LINUX, &["-Cforce-frame-pointers"]).unwrap().frame_pointers);
    assert!(!of(LINUX, &["-Cforce-frame-pointers=yes", "-Cforce-frame-pointers=no"]).unwrap().frame_pointers);
    let apple = of("aarch64-apple-darwin", &["-Cforce-frame-pointers=no"]).unwrap();
    assert!(apple.frame_pointers, "Apple's ABI keeps them");
    assert_eq!(apple.cpu, "apple-m1");
    assert!(of(LINUX, &["-Cforce-frame-pointers=sometimes"]).is_err());
}

#[test]
fn the_relocation_model_is_the_flags_or_the_targets() {
    let windows = "x86_64-pc-windows-msvc";
    assert_eq!(of(windows, &[]).unwrap().relocation, Relocation::Static);
    assert_eq!(of(windows, &["-C", "relocation-model=pic"]).unwrap().relocation, Relocation::Pic);
    assert_eq!(of(LINUX, &["-C", "relocation-model=static"]).unwrap().relocation, Relocation::Static);
    assert_eq!(of(LINUX, &["-C", "relocation-model=default"]).unwrap().relocation, Relocation::Pic);
}

#[test]
fn aarch64_features_are_past_its_baseline() {
    let apple = of("aarch64-apple-darwin", &["-C", "target-feature=-lse,+dotprod"]).unwrap();
    assert_eq!(apple.features, ["+dotprod", "-lse"]);
    let linux = of("aarch64-unknown-linux-gnu", &["-C", "target-feature=+lse"]).unwrap();
    assert_eq!((linux.cpu.as_str(), linux.features.as_slice()), ("generic", &["+lse".to_owned()][..]));
    let error = of("aarch64-unknown-linux-gnu", &["-C", "target-feature=+paca"]).unwrap_err();
    assert!(error.contains("pacg"), "{error}");
}

#[test]
fn native_is_the_hosts_and_only_for_the_host() {
    let other = if Target::host() == LINUX { "aarch64-unknown-linux-gnu" } else { LINUX };
    let error = of(other, &["-C", "target-cpu=native"]).unwrap_err();
    assert!(error.contains("native"), "{error}");
    if Target::parse(Target::host()).is_ok() {
        let native = of(Target::host(), &["-C", "target-cpu=native"]).unwrap();
        let base = of(Target::host(), &[]).unwrap();
        assert!(native.features.iter().all(|it| it.starts_with('+')), "{:?}", native.features);
        assert_eq!((native.target, native.arch), (base.target, base.arch));
    }
}

#[test]
fn here_is_the_hosts_features_on_the_baseline_cpu() {
    if !host_is_a_target() {
        return;
    }
    let here = here(&[]).unwrap();
    let native = of(Target::host(), &["-C", "target-cpu=native"]).unwrap();
    let elsewhere = of(Target::host(), &[]).unwrap();
    assert_eq!(here.features, native.features, "the features `native` gives");
    assert!(here.features.iter().all(|it| it.starts_with('+')), "{:?}", here.features);
    assert!(here.features.is_sorted());
    assert_eq!(here.cpu, elsewhere.cpu, "the baseline CPU's name, as rustc without `-C target-cpu`");
    assert_eq!((&here.target, &here.arch, &here.id), (&elsewhere.target, &elsewhere.arch, &elsewhere.id));
    assert_eq!(here.cpu, Target::parse(Target::host()).unwrap().cpu());
}

#[test]
fn elsewhere_is_the_baseline() {
    if !host_is_a_target() {
        return;
    }
    let elsewhere = of(Target::host(), &[]).unwrap();
    let apple = Target::host().contains("-apple-");
    assert!(elsewhere.features.is_empty(), "{:?}", elsewhere.features);
    assert_eq!(elsewhere.frame_pointers, apple);
    for other in [LINUX, "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
        assert!(of(other, &[]).unwrap().features.is_empty(), "{other}");
    }
}

#[test]
fn here_is_only_the_hosts_target() {
    let other = if Target::host() == LINUX { "aarch64-unknown-linux-gnu" } else { LINUX };
    let error = runs(other, &[], Runs::Here).unwrap_err();
    assert!(error.contains(other) && error.contains("where it is built"), "{error}");
}

#[test]
fn the_flags_win_over_where_it_runs() {
    if !host_is_a_target() {
        return;
    }
    let host = Target::host();
    let baseline = Target::parse(host).unwrap().cpu();
    let named = format!("target-cpu={baseline}");
    assert_eq!(here(&["-C", &named]).unwrap(), of(host, &["-C", &named]).unwrap());
    assert!(here(&["-C", &named]).unwrap().features.is_empty());
    let native = ["-C", "target-cpu=native"];
    assert_eq!(here(&native).unwrap(), of(host, &native).unwrap());
    let x86 = host.starts_with("x86_64-");
    let (on, off) = if x86 { ("+bmi2", "avx") } else { ("+dotprod", "lse") };
    let turned_on = here(&["-C", &format!("target-feature={on}")]).unwrap();
    assert!(turned_on.features.contains(&on.to_owned()), "{:?}", turned_on.features);
    let turned_off = here(&["-C", &format!("target-feature=-{off}")]).unwrap();
    assert!(!turned_off.features.contains(&format!("+{off}")), "{:?}", turned_off.features);
    if baseline_of(host).contains(&off) {
        assert!(turned_off.features.contains(&format!("-{off}")), "{:?}", turned_off.features);
    }
    if x86 {
        // Off with what implies it: `avx2`, `fma`, `f16c` and the AVX-512s.
        assert!(!turned_off.features.iter().any(|it| it.starts_with("+avx")), "{:?}", turned_off.features);
        let level = here(&["-C", "target-cpu=x86-64-v2"]).unwrap();
        assert_eq!(level, of(host, &["-C", "target-cpu=x86-64-v2"]).unwrap());
        assert!(!level.features.contains(&"+avx".to_owned()), "the level's, not the host's");
    }
}

fn baseline_of(host: &str) -> &'static [&'static str] {
    baseline(&Target::parse(host).unwrap())
}

#[test]
fn an_unknown_triple_is_an_error() {
    assert!(of("riscv64gc-unknown-linux-gnu", &[]).is_err());
}

#[test]
fn the_versions_are_the_ones_blaze_is_built_with() {
    let lock = include_str!("../../../../Cargo.lock");
    let at = lock.find("name = \"cranelift-codegen\"\nversion = \"").expect("cranelift-codegen in Cargo.lock");
    let version = lock[at..].split('"').nth(3).expect("its version");
    assert_eq!(version, CRANELIFT);
    match llvm() {
        Some(llvm) => {
            assert!(cfg!(feature = "llvm"));
            assert!(llvm.starts_with("23.") && llvm.split(' ').nth(1).is_some_and(|it| it.len() == 40), "{llvm}");
        }
        None => assert!(!cfg!(feature = "llvm")),
    }
}
