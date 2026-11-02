//! The toolchain a unit is compiled with ([`blaze_codegen::settings::Toolchain`]), from its
//! target and the `RUSTFLAGS` cargo gives it, as rustc reads them: `-C target-cpu`,
//! `-C target-feature`, `-C force-frame-pointers` and `-C relocation-model`. A flag that
//! changes code and is not read here is an error naming it; one that changes only the link
//! or what is reported is the link's or cargo's, and is not this module's. Where the program
//! runs ([`Runs`]) decides the features when the flags name no CPU.
//!
//! [`select`] then sets up the process's code generator for it, before the first unit.

#[cfg(test)]
mod tests;

use std::{collections::BTreeSet, path::Path, sync::OnceLock};

use blaze_codegen::{
    Arch,
    settings::{Relocation, Toolchain},
};
use blaze_ids::{ContentId, Hasher};

use crate::{ObjectFormat, Target};

const ID_DOMAIN: &str = "blaze v2 toolchain id 3";
const BUILD_DOMAIN: &str = "blaze v2 codegen build 1";

/// The nightly blaze is pinned to (`harness/toolchain`).
pub(crate) const NIGHTLY: &str = include_str!("../../../harness/toolchain").trim_ascii_end();

/// The version of Cranelift blaze is built with, as `Cargo.lock` has it (checked there).
const CRANELIFT: &str = "0.137.1";

/// The LLVM blaze pins (`llvm.json`).
const LLVM_PIN: &str = include_str!("../llvm.json");

/// rustc's x86-64 target features and those each turns on with it
/// (`rustc_target::target_features`).
const X86_IMPLIES: &[(&str, &[&str])] = &[
    ("adx", &[]),
    ("aes", &["sse2"]),
    ("avx", &["sse4.2"]),
    ("avx2", &["avx"]),
    ("avx512bf16", &["avx512bw"]),
    ("avx512bitalg", &["avx512bw"]),
    ("avx512bw", &["avx512f"]),
    ("avx512cd", &["avx512f"]),
    ("avx512dq", &["avx512f"]),
    ("avx512f", &["avx2", "fma", "f16c"]),
    ("avx512fp16", &["avx512bw"]),
    ("avx512ifma", &["avx512f"]),
    ("avx512vbmi", &["avx512bw"]),
    ("avx512vbmi2", &["avx512bw"]),
    ("avx512vl", &["avx512f"]),
    ("avx512vnni", &["avx512f"]),
    ("avx512vp2intersect", &["avx512f"]),
    ("avx512vpopcntdq", &["avx512f"]),
    ("avxifma", &["avx2"]),
    ("avxneconvert", &["avx2"]),
    ("avxvnni", &["avx2"]),
    ("avxvnniint16", &["avx2"]),
    ("avxvnniint8", &["avx2"]),
    ("f16c", &["avx"]),
    ("fma", &["avx"]),
    ("gfni", &["sse2"]),
    ("kl", &["sse2"]),
    ("pclmulqdq", &["sse2"]),
    ("sha", &["sse2"]),
    ("sha512", &["avx2"]),
    ("sm3", &["avx"]),
    ("sm4", &["avx2"]),
    ("sse2", &["sse"]),
    ("sse3", &["sse2"]),
    ("sse4.1", &["ssse3"]),
    ("sse4.2", &["sse4.1"]),
    ("sse4a", &["sse3"]),
    ("ssse3", &["sse3"]),
    ("vaes", &["avx2", "aes"]),
    ("vpclmulqdq", &["avx", "pclmulqdq"]),
    ("widekl", &["kl"]),
    ("xop", &["avx", "sse4a"]),
    ("xsavec", &["xsave"]),
    ("xsaveopt", &["xsave"]),
    ("xsaves", &["xsave"]),
];

/// rustc's AArch64 target features and those each turns on with it.
const AARCH64_IMPLIES: &[(&str, &[&str])] = &[
    ("aes", &["neon"]),
    ("dotprod", &["neon"]),
    ("fcma", &["neon"]),
    ("fhm", &["fp16"]),
    ("fp16", &["neon"]),
    ("i8mm", &["neon"]),
    ("jsconv", &["neon"]),
    ("rdm", &["neon"]),
    ("sha2", &["neon"]),
    ("sha3", &["sha2"]),
    ("sm4", &["neon"]),
    ("sve", &["neon"]),
    ("sve2", &["sve"]),
];

const X86_64: &[&str] = &["fxsr", "sse", "sse2", "x87"];
const X86_64_V2: &[&str] = &["cmpxchg16b", "lahfsahf", "popcnt", "sse3", "sse4.1", "sse4.2", "ssse3"];
const X86_64_V3: &[&str] = &["avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe", "xsave"];
const X86_64_V4: &[&str] = &["avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512vl"];
const AARCH64: &[&str] = &["fp-armv8", "neon"];
const APPLE_M1: &[&str] = &["fp-armv8", "lse", "neon"];

/// Where the program a unit is part of runs.
///
/// [`Toolchain::features`] are the code generator's only. `cfg(target_feature)` is not
/// computed from them, and must stay so: the front end's cfgs come from the flags alone
/// (`blaze_units::stdlib::target::CfgArgs`, and `blaze_cargo`'s `answers/own.rs` for
/// `--print cfg`), so a `cfg!` picks what rustc without `-C target-cpu=native` picks, whatever
/// the machine the code is compiled for has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Runs {
    /// On the machine that builds it: a dev build or a test that runs where it is built.
    /// With no `-C target-cpu`, its code may use every feature of this machine that
    /// Cranelift can, as `-C target-cpu=native` gives them, on the baseline CPU's name.
    Here,
    /// On another machine: a `--target`, remote or output build. With no `-C target-cpu`,
    /// the target's baseline.
    Elsewhere,
}

/// The toolchain of a unit for `target`, built with `rustflags` (its `RUSTFLAGS`, as cargo
/// gives them to rustc), for a program that `runs` there. A `-C target-cpu` or
/// `-C target-feature` in the flags wins over where it runs.
pub fn toolchain(target: &str, rustflags: &[String], runs: Runs) -> Result<Toolchain, String> {
    let emitted = Target::parse(target)?;
    let flags = Flags::read(rustflags)?;
    let arch = match emitted.arch {
        crate::Arch::X86_64 => Arch::X86_64,
        crate::Arch::Aarch64 => Arch::Aarch64,
        crate::Arch::Wasm32 => Arch::Wasm32,
    };
    let baseline = baseline(&emitted);
    let (cpu, mut on) = match flags.cpu.as_deref() {
        // alloc: per toolchain made, a few names
        None if runs == Runs::Elsewhere => (emitted.cpu().to_owned(), set(baseline)),
        None => {
            if emitted.triple() != Target::host() {
                // alloc: an error
                return Err(format!(
                    "a build that runs where it is built is for the host's target {}, not {}",
                    Target::host(),
                    emitted.triple()
                ));
            }
            // alloc: per toolchain made, a few names
            (emitted.cpu().to_owned(), on_host(&emitted, baseline))
        }
        Some("native") => native(&emitted, baseline)?,
        Some(named) => match levels(&emitted, named) {
            // alloc: per toolchain made, a few names
            Some(levels) => (named.to_owned(), set(&levels)),
            None => {
                // alloc: an error
                return Err(format!(
                    "-C target-cpu={named}: blaze knows the features of `native`, of {target}'s \
                     baseline `{}` and, on x86-64, of `x86-64-v2` to `x86-64-v4`, not of \
                     `{named}`",
                    emitted.cpu()
                ));
            }
        },
    };
    let implies = implications(&emitted);
    for (enable, name) in &flags.features {
        if *enable {
            turn_on(&mut on, implies, name);
        } else {
            turn_off(&mut on, implies, name);
        }
    }
    if emitted.arch == crate::Arch::Aarch64 && on.contains("paca") != on.contains("pacg") {
        return Err("-C target-feature: `paca` and `pacg` are enabled or disabled together, as \
                    rustc has them"
            // alloc: an error
            .to_owned());
    }
    let base = set(baseline);
    let mut features: Vec<String> = on
        .difference(&base)
        // alloc: per toolchain made, a feature's name each
        .map(|it| format!("+{it}"))
        // alloc: per toolchain made, a feature's name each
        .chain(base.difference(&on).map(|it| format!("-{it}")))
        // alloc: per toolchain made, its features
        .collect();
    features.sort_unstable();
    let frame_pointers = flags.frame_pointers.unwrap_or(false) || keeps_frame_pointers(&emitted);
    let relocation = match flags.relocation.as_deref() {
        None | Some("default") => default_relocation(&emitted),
        Some("pic" | "pie") => Relocation::Pic,
        Some("static") => Relocation::Static,
        Some(other) => {
            // alloc: an error
            return Err(format!(
                "-C relocation-model={other}: blaze's code is `pic`, `pie`, `static` or the \
                 target's default"
            ));
        }
    };
    Ok(Toolchain {
        id: id_of(&build()?),
        // alloc: per toolchain made, a few names
        target: target.to_owned(),
        arch,
        cpu,
        features,
        // alloc: per toolchain made, a few names
        cranelift: CRANELIFT.to_owned(),
        llvm: llvm(),
        stencils: match emitted.stencil() {
            Some(stencil) => {
                // alloc: an error, once per toolchain made
                let id = blaze_stencil::library_id(&stencil).map_err(|e| e.to_string())?;
                Some(blaze_format::hex::encode(&id))
            }
            None => None,
        },
        frame_pointers,
        relocation,
    })
}

/// Sets up the process's code generator for `toolchain`: its target and frame pointers, and
/// its target features as one more set the process compiles with. Said before the first unit
/// of each toolchain; each unit's spec is checked against it.
pub fn select(toolchain: &Toolchain) -> Result<(), String> {
    let target = Target::select(&toolchain.target)?;
    target.features_of(&toolchain.features)?;
    crate::backend::keep_frame_pointers(toolchain.frame_pointers);
    Ok(())
}

/// The pinned nightly blaze compiles against and the blaze build that makes the code (#1439):
/// the backends' versions are fields of their own.
fn id_of(build: &ContentId) -> ContentId {
    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.update(NIGHTLY.as_bytes()).update(b"\n").id(build);
    hasher.finish()
}

/// Which blaze build this process is: the hash of its executable's bytes, read once. The code
/// made for one code hash is what this blaze's front end and code generator make of it, and the
/// code hash reads neither (it is of the item's source, its callees by their interfaces), so a
/// store kept across blaze builds would hold another build's code under every key.
pub fn build() -> Result<ContentId, String> {
    static BUILD: OnceLock<Result<ContentId, String>> = OnceLock::new();
    BUILD
        .get_or_init(|| {
            // alloc: an error, once a process
            let exe = std::env::current_exe().map_err(|why| format!("the path of blaze itself: {why}"))?;
            build_of(&exe)
        })
        // clone: a content ID, or the error that stops every toolchain of the process
        .clone()
}

fn build_of(exe: &Path) -> Result<ContentId, String> {
    use std::io::Read as _;
    // alloc: an error, once a process
    let failed = |why: std::io::Error| format!("reading blaze itself, {}: {why}", exe.display());
    let mut file = std::fs::File::open(exe).map_err(failed)?;
    let mut hasher = Hasher::new(BUILD_DOMAIN);
    // alloc: one buffer, once a process
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut chunk).map_err(failed)?;
        if read == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&chunk[..read]);
    }
}

/// The pinned LLVM's version and commit; `None` for a blaze built without it.
fn llvm() -> Option<String> {
    if !cfg!(feature = "llvm") {
        return None;
    }
    let field = |key: &str| {
        // alloc: per toolchain made, a few names
        let after = LLVM_PIN.split_once(&format!("\"{key}\": \""))?.1;
        Some(after.split_once('"')?.0)
    };
    let (Some(version), Some(commit)) = (field("version"), field("commit")) else {
        panic!("llvm.json names no `version` and `commit`");
    };
    // alloc: per toolchain made, a few names
    Some(format!("{version} {commit}"))
}

/// What a CPU of the target always has, as rustc's baseline CPU of it ([`Target::cpu`]).
pub(super) fn baseline(target: &Target) -> &'static [&'static str] {
    match (target.arch, target.cpu()) {
        (crate::Arch::X86_64, _) => X86_64,
        (crate::Arch::Aarch64, "apple-m1") => APPLE_M1,
        (crate::Arch::Aarch64, _) => AARCH64,
        (crate::Arch::Wasm32, _) => &[],
    }
}

/// The features of a CPU named by `-C target-cpu` other than `native`: the baseline's, or an
/// x86-64 microarchitecture level's. `None` for another name.
fn levels(target: &Target, named: &str) -> Option<Vec<&'static str>> {
    if named == target.cpu() {
        // alloc: per toolchain made, a few names
        return Some(baseline(target).to_vec());
    }
    if target.arch != crate::Arch::X86_64 {
        return None;
    }
    let levels: &[&[&str]] = match named {
        "x86-64-v2" => &[X86_64, X86_64_V2],
        "x86-64-v3" => &[X86_64, X86_64_V2, X86_64_V3],
        "x86-64-v4" => &[X86_64, X86_64_V2, X86_64_V3, X86_64_V4],
        _ => return None,
    };
    Some(levels.concat())
}

/// `-C target-cpu=native`: the host's CPU by LLVM's name (the baseline's without LLVM), with
/// the features the host has that Cranelift's code may use.
fn native(target: &Target, baseline: &[&str]) -> Result<(String, BTreeSet<String>), String> {
    if target.triple() != Target::host() {
        // alloc: an error
        return Err(format!(
            "-C target-cpu=native names the host's CPU, and {} is not the host's target {}",
            target.triple(),
            Target::host()
        ));
    }
    let name = match crate::host_cpu() {
        Some((name, _)) => name,
        // alloc: per toolchain made, a few names
        None => target.cpu().to_owned(),
    };
    Ok((name, on_host(target, baseline)))
}

/// The baseline with every feature the host has that Cranelift's code may use, and what
/// each implies: `-C target-cpu=native`'s, and a build's that runs where it is built.
fn on_host(target: &Target, baseline: &[&str]) -> BTreeSet<String> {
    let mut on = set(baseline);
    for feature in host_features(target) {
        turn_on(&mut on, implications(target), feature);
    }
    on
}

/// The features this host has, of those past the baseline that Cranelift has a setting for
/// or that turn one on; none for a target of another architecture (wasm's code runs on no
/// CPU of its own).
fn host_features(target: &Target) -> Vec<&'static str> {
    let host = match target.arch {
        crate::Arch::X86_64 => cfg!(target_arch = "x86_64"),
        crate::Arch::Aarch64 => cfg!(target_arch = "aarch64"),
        crate::Arch::Wasm32 => false,
    };
    if !host {
        // alloc: none, an empty `Vec`
        return Vec::new();
    }
    // alloc: once per toolchain of the host's features, a few names
    let mut found = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        macro_rules! detect {
            ($($name:tt),*) => {
                $(if std::arch::is_x86_feature_detected!($name) { found.push($name); })*
            };
        }
        detect!(
            "sse3", "ssse3", "sse4.1", "sse4.2", "popcnt", "avx", "avx2", "fma", "f16c", "bmi1",
            "bmi2", "lzcnt", "avx512f", "avx512vl", "avx512dq", "avx512bw", "avx512cd",
            "avx512bitalg", "avx512vbmi", "avx512vnni"
        );
    }
    #[cfg(target_arch = "aarch64")]
    {
        macro_rules! detect {
            ($($name:tt),*) => {
                $(if std::arch::is_aarch64_feature_detected!($name) { found.push($name); })*
            };
        }
        detect!("lse", "fp16", "dotprod", "i8mm");
        // rustc has them on or off together.
        if std::arch::is_aarch64_feature_detected!("paca") && std::arch::is_aarch64_feature_detected!("pacg") {
            found.extend(["paca", "pacg"]);
        }
    }
    found
}

fn implications(target: &Target) -> &'static [(&'static str, &'static [&'static str])] {
    match target.arch {
        crate::Arch::X86_64 => X86_IMPLIES,
        crate::Arch::Aarch64 => AARCH64_IMPLIES,
        crate::Arch::Wasm32 => &[],
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    // alloc: per toolchain made, a few names
    names.iter().map(|it| (*it).to_owned()).collect()
}

/// `+name`: it and every feature it implies, at any depth.
fn turn_on(on: &mut BTreeSet<String>, implies: &[(&str, &[&str])], name: &str) {
    // alloc: per toolchain, the features still to turn on
    let mut todo = vec![name];
    while let Some(next) = todo.pop() {
        // alloc: per toolchain made, a few names
        if on.insert(next.to_owned()) {
            let implied = implies.iter().find(|(it, _)| *it == next).map_or(&[][..], |it| it.1);
            todo.extend_from_slice(implied);
        }
    }
}

/// `-name`: it and every feature that implies it, at any depth.
fn turn_off(on: &mut BTreeSet<String>, implies: &[(&str, &[&str])], name: &str) {
    // alloc: per toolchain, the features still to turn off
    let mut todo = vec![name];
    while let Some(next) = todo.pop() {
        on.remove(next);
        for &(feature, implied) in implies {
            if implied.contains(&next) && on.contains(feature) {
                todo.push(feature);
            }
        }
    }
}

/// Whether the target keeps frame pointers whatever the flags say: Apple's ABIs require them.
fn keeps_frame_pointers(target: &Target) -> bool {
    target.triple().contains("-apple-")
}

/// rustc's relocation model of the target: PE images are reached relative to the
/// instruction without a table, everything else is position-independent.
fn default_relocation(target: &Target) -> Relocation {
    match target.object_format {
        ObjectFormat::Coff => Relocation::Static,
        ObjectFormat::Elf | ObjectFormat::MachO | ObjectFormat::Wasm => Relocation::Pic,
    }
}

/// What the flags say of the code, in the order given: a later `-C` of the same option
/// wins, and `-C target-feature`s add up.
#[derive(Default, Debug, PartialEq, Eq)]
struct Flags {
    cpu: Option<String>,
    /// Each feature named, `true` for `+`.
    features: Vec<(bool, String)>,
    frame_pointers: Option<bool>,
    relocation: Option<String>,
}

/// The flags of rustc that take the next word as their value, when not given it after `=`
/// or joined: their value is not a flag.
const TAKES_VALUE: &[&str] = &[
    "--cfg", "--check-cfg", "-L", "-l", "--extern", "--cap-lints", "-A", "-W", "-D", "-F",
    "--allow", "--warn", "--deny", "--forbid", "--force-warn", "--edition", "--crate-type",
    "--crate-name", "--emit", "--print", "-o", "--out-dir", "--target", "--sysroot",
    "--error-format", "--json", "--color", "--explain", "--diagnostic-width", "--env-set",
    "-Z", "--remap-path-scope",
];

/// The `-C` options that change only the link, the output's names or what is reported:
/// the link's and cargo's, not the code's.
const NOT_CODE: &[&str] = &[
    "ar", "codegen-units", "default-linker-libraries", "dlltool", "embed-bitcode",
    "extra-filename", "incremental", "inline-threshold", "link-arg", "link-args",
    "link-dead-code", "link-self-contained", "linker", "linker-flavor", "metadata",
    "no-stack-check", "prefer-dynamic", "relro-level", "remark", "rpath", "save-temps",
    "split-debuginfo", "strip", "unsafe-allow-abi-mismatch",
];

/// The `-C` options the profile says: blaze compiles with the profile's.
const PROFILE: &[(&str, &str)] = &[
    ("opt-level", "opt-level"),
    ("debuginfo", "debug"),
    ("debug-assertions", "debug-assertions"),
    ("overflow-checks", "overflow-checks"),
    ("panic", "panic"),
];

/// The `-C` options that change code and are off unless asked: blaze's code is as with
/// them off, so only an off value is taken.
const OFF: &[&str] = &[
    "control-flow-guard", "instrument-coverage", "linker-plugin-lto", "lto",
    "no-prepopulate-passes", "no-redzone", "no-vectorize-loops", "no-vectorize-slp",
    "soft-float",
];

fn is_no(value: Option<&str>) -> bool {
    matches!(value, Some("n" | "no" | "off" | "false"))
}

fn is_yes(value: Option<&str>) -> bool {
    matches!(value, None | Some("y" | "yes" | "on" | "true"))
}

impl Flags {
    fn read(rustflags: &[String]) -> Result<Flags, String> {
        let mut flags = Flags::default();
        let mut words = rustflags.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let option = match word {
                "-C" | "--codegen" => {
                    let Some(value) = words.next() else {
                        // alloc: an error
                        return Err(format!("`{word}` ends the flags without an option"));
                    };
                    value
                }
                "-O" => return Err(profile_error("-O", "opt-level")),
                "-g" => return Err(profile_error("-g", "debug")),
                "--remap-path-prefix" => return Err(remap_error()),
                _ if word.starts_with("--remap-path-prefix=") => return Err(remap_error()),
                _ if word.starts_with("--codegen=") => &word["--codegen=".len()..],
                _ if word.starts_with("-C") => &word[2..],
                _ => {
                    if TAKES_VALUE.contains(&word) {
                        words.next();
                    }
                    continue;
                }
            };
            flags.option(option)?;
        }
        Ok(flags)
    }

    /// One `-C name[=value]`.
    fn option(&mut self, option: &str) -> Result<(), String> {
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        // alloc: per `-C` read, its name as rustc spells it (`_` is `-`)
        let name = name.replace('_', "-");
        let shown = || match value {
            // alloc: an error's flag as written
            Some(value) => format!("-C {name}={value}"),
            // alloc: an error's flag as written
            None => format!("-C {name}"),
        };
        match name.as_str() {
            "target-cpu" => {
                let cpu = value.filter(|it| !it.is_empty());
                // alloc: an error
                self.cpu = Some(cpu.ok_or_else(|| format!("{}: no CPU named", shown()))?.to_owned());
            }
            "target-feature" => {
                for feature in value.unwrap_or_default().split(',').filter(|it| !it.is_empty()) {
                    let (enable, feature_name) = match feature.split_at_checked(1) {
                        Some(("+", it)) if !it.is_empty() => (true, it),
                        Some(("-", it)) if !it.is_empty() => (false, it),
                        _ => {
                            // alloc: an error
                            return Err(format!(
                                "-C target-feature: `{feature}` is not `+` or `-` and a feature"
                            ));
                        }
                    };
                    // The C runtime linked statically: the link's, not the code's.
                    if feature_name != "crt-static" {
                        // alloc: per toolchain made, a few names
                        self.features.push((enable, feature_name.to_owned()));
                    }
                }
            }
            "force-frame-pointers" => {
                self.frame_pointers = Some(match value {
                    _ if is_yes(value) => true,
                    Some("always" | "non-leaf") => true,
                    Some("none") => false,
                    _ if is_no(value) => false,
                    // alloc: an error
                    _ => return Err(format!("{}: not a value rustc takes", shown())),
                });
            }
            "relocation-model" => {
                let model = value.filter(|it| !it.is_empty());
                self.relocation =
                    // alloc: an error
                    Some(model.ok_or_else(|| format!("{}: no model named", shown()))?.to_owned());
            }
            "passes" => {
                let passes = value.unwrap_or_default().split([' ', ',']).filter(|it| !it.is_empty());
                for pass in passes {
                    if pass != "sancov-module" {
                        // alloc: an error
                        return Err(format!(
                            "{}: blaze runs LLVM's passes of the profile and `sancov-module` \
                             (SanitizerCoverage), not `{pass}`",
                            shown()
                        ));
                    }
                }
            }
            "llvm-args" => {
                for arg in value.unwrap_or_default().split(' ').filter(|it| !it.is_empty()) {
                    if !arg.starts_with("-sanitizer-coverage-") {
                        // alloc: an error
                        return Err(format!(
                            "{}: blaze gives LLVM SanitizerCoverage's `-sanitizer-coverage-*` \
                             only, not `{arg}`",
                            shown()
                        ));
                    }
                }
            }
            "jump-tables" if is_yes(value) => {}
            _ if NOT_CODE.contains(&name.as_str()) => {}
            _ if OFF.contains(&name.as_str()) && is_no(value) => {}
            _ => {
                if let Some((_, key)) = PROFILE.iter().find(|(it, _)| *it == name) {
                    return Err(profile_error(&shown(), key));
                }
                // alloc: an error
                return Err(format!(
                    "{}: blaze does not read this flag of rustc, which changes the code",
                    shown()
                ));
            }
        }
        Ok(())
    }
}

fn profile_error(flag: &str, key: &str) -> String {
    // alloc: an error
    format!(
        "`{flag}` in RUSTFLAGS: blaze compiles with the profile's `{key}`; set it in \
         `[profile.*]` instead"
    )
}

fn remap_error() -> String {
    "--remap-path-prefix: blaze names a file by its path in its package (`file!()`, panic \
     locations) and remaps nothing"
        // alloc: an error
        .to_owned()
}
