use super::*;

fn of(target: &str, flags: &[&str]) -> Result<Toolchain, String> {
    runs(target, flags, Runs::Elsewhere)
}

fn here(flags: &[&str]) -> Result<Toolchain, String> {
    runs(Target::host(), flags, Runs::Here)
}

/// The code generator these tests make toolchains as: no program hands one over to a test.
const CODE: CodegenCode = CodegenCode(ContentId([7; 32]));

fn runs(target: &str, flags: &[&str], runs: Runs) -> Result<Toolchain, String> {
    let flags: Vec<String> = flags.iter().map(|it| (*it).to_owned()).collect();
    toolchain_for(&CODE, target, &fixture_target_data(target), &flags, runs)
}

/// Whether blaze emits code for the machine the tests run on.
fn host_is_a_target() -> bool {
    Target::of(Target::host()).is_ok()
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

/// The code under a code hash is what this code generator makes of it: another code generator
/// is another toolchain, and so a store kept across blaze builds holds no other generator's
/// template under the key of this one's. A blaze build with the code generator's code
/// unchanged (an edit to the CLI) is the same toolchain: its id reads no executable.
#[test]
fn another_code_generator_is_another_toolchain() {
    assert!(
        NIGHTLY.starts_with("nightly-") && !NIGHTLY.contains(char::is_whitespace),
        "{NIGHTLY:?}"
    );
    let this = of(LINUX, &[]).unwrap();
    assert_eq!(this.id, id_of(&CODE, &fixture_target_data(LINUX)));
    let mut other = CODE;
    other.0.0[0] ^= 1;
    let flags: [String; 0] = [];
    let elsewhere =
        toolchain_for(&other, LINUX, &fixture_target_data(LINUX), &flags, Runs::Elsewhere).unwrap();
    assert_ne!(elsewhere.id, this.id);
    assert_eq!(Toolchain { id: this.id, ..elsewhere }, this, "the code is the id alone");
}

/// C3 of the review: the data layout the front end lays a unit's types out by is read from
/// data past the code generator's packages (blaze-units' recorded target spec), so the code
/// identity does not cover it: the toolchain's id does. Two target datas differing in their
/// layout alone are two toolchains, and a toolchain is made only of its own target's data.
#[test]
fn another_data_layout_is_another_toolchain() {
    let flags: [String; 0] = [];
    let this =
        toolchain_for(&CODE, LINUX, &fixture_target_data(LINUX), &flags, Runs::Elsewhere).unwrap();
    let mut other = fixture_target_data(LINUX);
    other.data_layout =
        "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128".into();
    let relaid = toolchain_for(&CODE, LINUX, &other, &flags, Runs::Elsewhere).unwrap();
    assert_ne!(relaid.id, this.id, "the data layout is in the toolchain's id");
    assert_eq!(Toolchain { id: this.id, ..relaid }, this, "the layout is the id alone");
    let why = toolchain_for(
        &CODE,
        LINUX,
        &fixture_target_data("aarch64-unknown-linux-gnu"),
        &flags,
        Runs::Elsewhere,
    )
    .unwrap_err();
    assert!(why.contains(LINUX) && why.contains("aarch64-unknown-linux-gnu"), "{why}");
}

/// A process makes no toolchain before its code generator's code identity is handed over, and
/// takes no second, other one: no key falls back to the executable's bytes.
#[test]
fn the_code_identity_is_handed_over_once() {
    // This test binary's one hand-over: every other test names its code itself.
    let code = CodegenCode(ContentId([9; 32]));
    match code_or_unset() {
        None => {
            assert!(
                toolchain(LINUX, &fixture_target_data(LINUX), &[], Runs::Elsewhere)
                    .unwrap_err()
                    .contains("BLAZE_CODEGEN_CODE")
            );
            set_code(code).unwrap();
        }
        Some(it) => assert_eq!(it, code),
    }
    set_code(code).unwrap();
    let mut other = code;
    other.0.0[1] ^= 1;
    let refused = set_code(other).unwrap_err();
    assert!(
        refused.contains(&format!("{:x}", code.0)) && refused.contains(&format!("{:x}", other.0)),
        "{refused}"
    );
    assert_eq!(
        toolchain(LINUX, &fixture_target_data(LINUX), &[], Runs::Elsewhere).unwrap().id,
        id_of(&code, &fixture_target_data(LINUX))
    );
}

fn code_or_unset() -> Option<CodegenCode> {
    code().ok()
}

#[test]
fn a_feature_turns_on_what_it_implies_and_off_what_implies_it() {
    let on = of(LINUX, &["-C", "target-feature=+avx2"]).unwrap();
    assert_eq!(on.features, ["+avx", "+avx2", "+sse3", "+sse4.1", "+sse4.2", "+ssse3"]);
    let off = of(LINUX, &["-Ctarget-feature=+avx2", "-Ctarget-feature=-avx"]).unwrap();
    assert_eq!(off.features, ["+sse3", "+sse4.1", "+sse4.2", "+ssse3"]);
    let listed = of(LINUX, &["--codegen", "target-feature=+fma,+bmi2"]).unwrap();
    assert!(
        listed.features.contains(&"+fma".to_owned())
            && listed.features.contains(&"+bmi2".to_owned())
    );
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

/// Any CPU rustc takes is its features from the CPU table, past the target's baseline, under
/// its own name: an LLVM alias (`apple-m1`) is the baseline it names, and a CPU that lacks a
/// feature of the baseline (`apple-a7`, no LSE) turns it off.
#[test]
fn a_named_cpu_is_the_tables_features_past_the_baseline() {
    let n1 = of("aarch64-unknown-linux-gnu", &["-C", "target-cpu=neoverse-n1"]).unwrap();
    assert_eq!(n1.cpu, "neoverse-n1");
    for feature in ["+dotprod", "+lse", "+rcpc"] {
        assert!(n1.features.contains(&feature.to_owned()), "{feature}: {:?}", n1.features);
    }
    let darwin = "aarch64-apple-darwin";
    let m4 = of(darwin, &["-C", "target-cpu=apple-m4"]).unwrap();
    assert_eq!(m4.cpu, "apple-m4");
    assert!(m4.features.contains(&"+sme".to_owned()), "{:?}", m4.features);
    assert!(m4.features.iter().all(|it| it.starts_with('+')), "{:?}", m4.features);
    let m1 = of(darwin, &["-C", "target-cpu=apple-m1"]).unwrap();
    assert!(m1.features.is_empty(), "the baseline: {:?}", m1.features);
    let a7 = of(darwin, &["-C", "target-cpu=apple-a7"]).unwrap();
    assert!(a7.features.contains(&"-lse".to_owned()), "{:?}", a7.features);
    assert_eq!(a7.lacks_baseline(), Some("lse"));
}

#[test]
fn flags_of_the_link_and_of_reports_are_not_the_codes() {
    let flags = [
        "-C",
        "link-arg=-fuse-ld=lld",
        "--cfg",
        "tokio_unstable",
        "-Dwarnings",
        "-Ccodegen-units=1",
        "-C",
        "lto=off",
        "-Ctarget-feature=+crt-static",
        "-L",
        "-Cnot-a-flag",
        "-Zshare-generics",
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
        (&["-C", "target-cpu=pentium-m99"][..], "pentium-m99"),
        (&["-C", "target-feature=avx2"][..], "avx2"),
        (&["--remap-path-prefix=/a=/b"][..], "remap-path-prefix"),
        (&["-C", "relocation-model=ropi"][..], "ropi"),
        (&["-C", "passes=gvn"][..], "gvn"),
        (&["-C", "llvm-args=-x86-asm-syntax=intel"][..], "-x86-asm-syntax"),
        (&["-C", "save-temps"][..], "save-temps"),
        (&["-C", "remark=all"][..], "remark"),
        (&["-C"][..], "-C"),
    ] {
        let error = of(LINUX, flags).unwrap_err();
        assert!(error.contains(named), "{flags:?}: {error}");
    }
    assert!(
        of(LINUX, &["-C", "passes=sancov-module", "-C", "llvm-args=-sanitizer-coverage-level=3"])
            .is_ok()
    );
}

#[test]
fn frame_pointers_are_the_flags_or_the_targets() {
    assert!(of(LINUX, &["--codegen", "force_frame_pointers=yes"]).unwrap().frame_pointers);
    assert!(of(LINUX, &["-Cforce-frame-pointers"]).unwrap().frame_pointers);
    assert!(
        !of(LINUX, &["-Cforce-frame-pointers=yes", "-Cforce-frame-pointers=no"])
            .unwrap()
            .frame_pointers
    );
    // Apple's frame records are its code's own (`Target::llvm_frame_pointer`), not forced.
    let apple = of("aarch64-apple-darwin", &["-Cforce-frame-pointers=no"]).unwrap();
    assert!(!apple.frame_pointers, "only the flags force every frame pointer");
    assert!(of("aarch64-apple-darwin", &["-Cforce-frame-pointers=yes"]).unwrap().frame_pointers);
    assert_eq!(apple.cpu, "apple-m1");
    assert!(of(LINUX, &["-Cforce-frame-pointers=sometimes"]).is_err());
}

#[test]
fn the_relocation_model_is_the_flags_or_the_targets() {
    let windows = "x86_64-pc-windows-msvc";
    assert_eq!(of(windows, &[]).unwrap().relocation, Relocation::Static);
    assert_eq!(of(windows, &["-C", "relocation-model=pic"]).unwrap().relocation, Relocation::Pic);
    assert_eq!(
        of(LINUX, &["-C", "relocation-model=static"]).unwrap().relocation,
        Relocation::Static
    );
    assert_eq!(of(LINUX, &["-C", "relocation-model=default"]).unwrap().relocation, Relocation::Pic);
}

#[test]
fn aarch64_features_are_past_its_baseline() {
    let apple = of("aarch64-apple-darwin", &["-C", "target-feature=-lse,+dotprod"]).unwrap();
    assert_eq!(apple.features, ["+dotprod", "-lse"]);
    let linux = of("aarch64-unknown-linux-gnu", &["-C", "target-feature=+lse"]).unwrap();
    assert_eq!(
        (linux.cpu.as_str(), linux.features.as_slice()),
        ("generic", &["+lse".to_owned()][..])
    );
    let error = of("aarch64-unknown-linux-gnu", &["-C", "target-feature=+paca"]).unwrap_err();
    assert!(error.contains("pacg"), "{error}");
}

#[test]
fn native_is_the_hosts_and_only_for_the_host() {
    let other = if Target::host() == LINUX { "aarch64-unknown-linux-gnu" } else { LINUX };
    let error = of(other, &["-C", "target-cpu=native"]).unwrap_err();
    assert!(error.contains("native"), "{error}");
    if Target::of(Target::host()).is_ok() {
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
    for feature in &here.features {
        let given = &native.features;
        assert!(given.contains(feature), "{feature}: of those `native` gives, {given:?}");
    }
    assert!(here.features.iter().all(|it| it.starts_with('+')), "{:?}", here.features);
    assert!(here.features.is_sorted());
    assert_eq!(
        here.cpu, elsewhere.cpu,
        "the baseline CPU's name, as rustc without `-C target-cpu`"
    );
    assert_eq!(
        (&here.target, &here.arch, &here.id),
        (&elsewhere.target, &elsewhere.arch, &elsewhere.id)
    );
    assert_eq!(here.cpu, Target::of(Target::host()).unwrap().cpu());
}

#[test]
fn elsewhere_is_the_baseline() {
    if !host_is_a_target() {
        return;
    }
    let elsewhere = of(Target::host(), &[]).unwrap();
    assert!(elsewhere.features.is_empty(), "{:?}", elsewhere.features);
    assert!(!elsewhere.frame_pointers, "no flag forces them, on Apple's targets too");
    for other in
        [LINUX, "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
    {
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
    let baseline = Target::of(host).unwrap().cpu();
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
        assert!(
            !turned_off.features.iter().any(|it| it.starts_with("+avx")),
            "{:?}",
            turned_off.features
        );
        let level = here(&["-C", "target-cpu=x86-64-v2"]).unwrap();
        assert_eq!(level, of(host, &["-C", "target-cpu=x86-64-v2"]).unwrap());
        assert!(!level.features.contains(&"+avx".to_owned()), "the level's, not the host's");
    }
}

fn baseline_of(host: &str) -> &'static [&'static str] {
    baseline(Target::of(host).unwrap())
}

#[test]
fn an_unknown_triple_is_an_error() {
    assert!(of("riscv64gc-unknown-linux-gnu", &[]).is_err());
}

#[test]
fn the_versions_are_the_ones_blaze_is_built_with() {
    let lock = include_str!("../../../../Cargo.lock");
    let at = lock
        .find("name = \"cranelift-codegen\"\nversion = \"")
        .expect("cranelift-codegen in Cargo.lock");
    let version = lock[at..].split('"').nth(3).expect("its version");
    assert_eq!(version, CRANELIFT);
    match llvm() {
        Some(llvm) => {
            assert!(cfg!(feature = "llvm"));
            assert!(
                llvm.starts_with("23.") && llvm.split(' ').nth(1).is_some_and(|it| it.len() == 40),
                "{llvm}"
            );
        }
        None => assert!(!cfg!(feature = "llvm")),
    }
}

/// Rule 26 and the point of the code identity: a dev template one blaze build kept is read by
/// the next after a restart of the store, when the next differs from it only outside the code
/// generator (its CLI), and by no build whose code generator differs. The two blaze builds are
/// `blaze_codegen_code`'s made-up workspace before and after each edit, their codes worked out
/// as blaze-engine's build script works them out.
#[test]
fn a_template_kept_by_one_blaze_build_is_read_by_the_next() {
    use blaze_codegen::settings::{DebugInfo, Panic, Pgo, Sanitize, Spec};
    let workspace = blaze_codegen_code::fixture::Workspace::new("restart", false);
    let built = workspace.code();
    workspace.write("crates/cli/src/main.rs", "fn main() { println!(\"another CLI\") }\n");
    let cli_edited = workspace.code();
    workspace.write("crates/front/src/lib.rs", "pub fn lower() { /* another front end */ }\n");
    let codegen_edited = workspace.code();

    let stored =
        blaze_codegen::InstanceKey(ContentId::of("blaze v2 toolchain test instance", b"main"));
    let key = |code: [u8; 32]| {
        let toolchain = toolchain_for(
            &CodegenCode(ContentId(code)),
            LINUX,
            &fixture_target_data(LINUX),
            &[],
            Runs::Elsewhere,
        )
        .unwrap();
        let spec = Spec {
            toolchain,
            backend: blaze_codegen::Backend::Stencil,
            overflow_checks: true,
            debug_assertions: true,
            panic: Panic::Unwind,
            debuginfo: DebugInfo::None,
            fold: false,
            counted: true,
            mir_inline: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
            proc_macro_bridge: false,
            heap_to_stack: false,
            report: false,
            fuel: false,
        };
        blaze_codegen::kernel::template(&stored, &spec.settings(), &spec.toolchain.id)
    };
    // Rule 25: one key for one code, whenever it is worked out.
    assert_eq!(key(built), key(built));
    assert_eq!(
        key(cli_edited),
        key(built),
        "a blaze rebuilt with only its CLI edited keys templates alike"
    );
    assert_ne!(
        key(codegen_edited),
        key(built),
        "a blaze whose code generator changed keys templates apart"
    );

    let root = std::env::temp_dir()
        .join(format!("blaze-codegen-code-restart-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let template = b"the bytes of one dev template".to_vec();
    {
        let mut driven = blaze_cas::Driven::open(&root).unwrap();
        driven.store().put(key(built), blaze_cas::Kind::Code, &template).unwrap();
        driven.flush().unwrap();
    }
    let mut driven = blaze_cas::Driven::open(&root).unwrap();
    let got = driven
        .store()
        .get(&key(cli_edited))
        .unwrap()
        .expect("the store came back without the template the last blaze build kept");
    assert_eq!(got.bytes, template.as_slice());
    assert!(driven.store().get(&key(codegen_edited)).unwrap().is_none());
    drop(driven);
    std::fs::remove_dir_all(&root).unwrap();
}
