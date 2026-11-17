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

use std::collections::BTreeSet;

use base_db::target::TargetData;
use blaze_codegen::{
    Arch,
    settings::{Relocation, Toolchain},
};
use blaze_ids::{ContentId, Hasher};

use crate::{ObjectFormat, Target};

const ID_DOMAIN: &str = "blaze v2 toolchain id 5";

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
    /// With no `-C target-cpu`, its code may use every feature this machine is found to have
    /// of those the code generator picks instructions by (`host_features`), on the baseline
    /// CPU's name: of the features `-C target-cpu=native` gives, without its CPU's name.
    Here,
    /// On another machine: a `--target`, remote or output build. With no `-C target-cpu`,
    /// the target's baseline.
    Elsewhere,
}

/// The toolchain of a unit for `target`, built with `rustflags` (its `RUSTFLAGS`, as cargo
/// gives them to rustc), for a program that `runs` there. A `-C target-cpu` or
/// `-C target-feature` in the flags wins over where it runs. `target_data` is what the front end
/// lays the unit's types out by (its crates' `TargetData`, the data layout from blaze-units'
/// recorded spec): part of the id, as code made under one layout is not code for another.
pub fn toolchain(
    target: &str,
    target_data: &TargetData,
    rustflags: &[String],
    runs: Runs,
) -> Result<Toolchain, String> {
    toolchain_for(&code()?, target, target_data, rustflags, runs)
}

/// [`toolchain`], made by the code generator whose code is `code` rather than this process's.
pub fn toolchain_for(
    code: &CodegenCode,
    target: &str,
    target_data: &TargetData,
    rustflags: &[String],
    runs: Runs,
) -> Result<Toolchain, String> {
    if target_data.tuple.as_str() != target {
        // alloc: an error
        return Err(format!(
            "the toolchain for `{target}` is given the target data of `{}`: a unit's code is laid out by \
             its own target's",
            target_data.tuple.as_str()
        ));
    }
    let emitted = Target::of(target)?;
    let flags = Flags::read(rustflags)?;
    let arch = match emitted.arch {
        crate::Arch::X86_64 => Arch::X86_64,
        crate::Arch::Aarch64 => Arch::Aarch64,
        crate::Arch::Wasm32 => Arch::Wasm32,
        crate::Arch::Wasm64 => Arch::Wasm64,
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
        // alloc: per toolchain made, a few names
        Some(named) => (named.to_owned(), of_cpu(&emitted, named)?),
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
    // Only what the flags force: the frame records the target's ABI wants of every function
    // that calls are its code's own, whatever this says (`Target::requires_frame_pointers`,
    // `Target::llvm_frame_pointer`), as rustc's `-C force-frame-pointers=no` cannot take
    // Apple's away.
    let frame_pointers = flags.frame_pointers.unwrap_or(false);
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
        id: id_of(code, target_data),
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

/// Sets up the process's code generator for `toolchain`: its target as one more the process
/// compiles for (each unit names its own, `backend::unit_target`), and its target features as
/// one more set the process compiles with. Said before the first unit of each toolchain; each
/// unit's spec is checked against it. Nothing of it is process-wide: frame pointers are each
/// unit's own, read where its code is made (`v2::Function::frame_pointers`).
pub fn select(toolchain: &Toolchain) -> Result<(), String> {
    let target = Target::of(&toolchain.target)?;
    target.features_of(&toolchain.cpu, &toolchain.features)?;
    Ok(())
}

/// The content identity of the code that makes blaze's generated code: of every package the
/// code generator (`hir-ty-codegen`, the front end it reads and the backends it drives) is
/// built from, with the LLVM it links and how blaze itself was built. blaze-engine's build
/// script works it out (`blaze_codegen_code::emit`, crates/blaze-codegen-code) and the program hands it
/// over with [`set_code`] before any command runs, so an edit to code outside the code
/// generator (the CLI, the daemon, the lints) keeps it, and every template stored with it.
///
/// TEMPORARY in its granularity: an edit anywhere in a package of the code generator changes
/// it, so a change to codegen misses every template, not only those it affects. It is
/// replaced, not kept beside, by the code generator's behaviour identity,
/// `core::reflect::fn_info(<codegen entry>).code_id` (docs/v2/plan/design/code-identity.md,
/// "Toolchain::id"), once const eval answers `fn_info` and blaze builds itself with
/// `blaze_comptime`. No key falls back to the executable's bytes: a process that was handed no
/// code identity makes no toolchain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodegenCode(pub ContentId);

impl CodegenCode {
    /// Of the 64 hex digits `blaze_codegen_code::emit` sets `BLAZE_CODEGEN_CODE` to.
    pub fn from_hex(hex: &str) -> Result<CodegenCode, String> {
        let refused = || {
            format!(
                "the code generator's code identity `{hex}` is not 64 hex digits (`BLAZE_CODEGEN_CODE`)"
            )
        };
        if hex.len() != 64 {
            return Err(refused());
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| refused())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| refused())?;
        }
        Ok(CodegenCode(ContentId(bytes)))
    }
}

#[expect(
    clippy::disallowed_types,
    reason = "which code generator the process is, handed over once by the program's main so that no edit outside the code generator changes this crate"
)]
static CODE: std::sync::OnceLock<CodegenCode> = std::sync::OnceLock::new();

/// Hands over the code identity of the process's code generator: once, before the first
/// toolchain. Handing over another one after is refused, naming both: one process is one code
/// generator.
pub fn set_code(code: CodegenCode) -> Result<(), String> {
    match CODE.get_or_init(|| code) {
        it if *it == code => Ok(()),
        // alloc: an error, once
        it => Err(format!(
            "the code generator's code identity is {:x} already, and {:x} was handed over: one \
             process is one code generator",
            it.0, code.0
        )),
    }
}

/// The code identity [`set_code`] was handed, or an error naming who should have: a program
/// or test that makes toolchains hands its own over first, as blaze-engine's main does from its
/// build script's `BLAZE_CODEGEN_CODE`.
pub fn code() -> Result<CodegenCode, String> {
    CODE.get().copied().ok_or_else(|| {
        "the code generator was handed no code identity (`hir_ty_codegen::v2::set_code`): the \
         program that runs it hands over its build script's `BLAZE_CODEGEN_CODE` before any \
         toolchain is made, and blaze keys no code by the executable's bytes in its place"
            // alloc: an error
            .to_owned()
    })
}

/// The target data of a test's made-up crates for `tuple`: x86-64 Linux's data layout, as the
/// front end's fixtures give it, under `tuple`'s name.
#[cfg(test)]
pub(crate) fn fixture_target_data(tuple: &str) -> TargetData {
    TargetData {
        tuple: intern::Symbol::intern(tuple),
        data_layout: "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128"
            .into(),
        arch: base_db::target::Arch::Other,
        has_thumb_interworking: false,
    }
}

/// `Toolchain::id`: the pinned nightly blaze compiles against, the code generator's code, the
/// target data the front end lays types out by (`TargetData::content`: the tuple, the data
/// layout, the arch; read from data past the code generator's packages, so not in its code)
/// and what of this crate's own build changes the code it makes (its features and debug
/// assertions, which the build script, run for blaze-engine, does not see). The backends'
/// versions are fields of their own.
fn id_of(code: &CodegenCode, target_data: &TargetData) -> ContentId {
    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.part(NIGHTLY.as_bytes()).id(&code.0).id(&target_data.content());
    hasher.update(&[
        u8::from(cfg!(feature = "llvm")),
        u8::from(cfg!(feature = "llvm-wasm")),
        u8::from(cfg!(debug_assertions)),
    ]);
    hasher.finish()
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
        (crate::Arch::Wasm32 | crate::Arch::Wasm64, _) => &[],
    }
}

/// The features of CPU `named` of `target` (`-C target-cpu`, any CPU rustc takes, LLVM's
/// aliases among them): the baseline's, with what the CPU has past the target's baseline CPU
/// turned on and what it lacks of it turned off, each with what it implies. What a CPU has is
/// the CPU table's (`target_features::cpus`, rustc's features of each CPU, the one home of
/// what blaze knows of CPUs); a name it does not know is an error naming it and the target.
/// Only the difference between the two CPUs is read, so the baseline's own names (`fp-armv8`,
/// which rustc folds into `neon`) stay as Cranelift and LLVM know them.
fn of_cpu(target: &Target, named: &str) -> Result<BTreeSet<String>, String> {
    let baseline = baseline(target);
    if named == target.cpu() {
        return Ok(set(baseline));
    }
    // alloc: an error, naming the target and the CPU
    let has = target_features::cpus::features(target.triple(), named).map_err(|e| e.to_string())?;
    let base = target_features::cpus::features(target.triple(), target.cpu()).map_err(|e| {
        // alloc: an error
        format!("{e}: the target's baseline CPU, which `-C target-cpu={named}` is read against")
    })?;
    let implies = implications(target);
    let mut on = set(baseline);
    for &feature in has.iter().filter(|it| !base.contains(it)) {
        turn_on(&mut on, implies, feature);
    }
    for &feature in base.iter().filter(|it| !has.contains(it)) {
        turn_off(&mut on, implies, feature);
    }
    Ok(on)
}

/// `-C target-cpu=native`: the host's CPU by LLVM's name (`apple-m4`, `znver4`; the key
/// records it, never `native`) with its features as [`of_cpu`] gives them, less those LLVM
/// finds the host without (a VM that hides AVX-512 from a CPU that has it), and with every
/// feature the host is found to have. Without LLVM (the seed, whose native code is stencils
/// only) the host has no name to read: the baseline's name with what the host is found to
/// have ([`on_host`]).
fn native(target: &Target, baseline: &[&str]) -> Result<(String, BTreeSet<String>), String> {
    if target.triple() != Target::host() {
        // alloc: an error
        return Err(format!(
            "-C target-cpu=native names the host's CPU, and {} is not the host's target {}",
            target.triple(),
            Target::host()
        ));
    }
    let Some((name, told)) = crate::host_cpu() else {
        // alloc: per toolchain made, a few names
        return Ok((target.cpu().to_owned(), on_host(target, baseline)));
    };
    let mut on = of_cpu(target, &name).map_err(|e| {
        // alloc: an error
        format!("-C target-cpu=native: LLVM names the host's CPU `{name}`, and {e}")
    })?;
    let implies = implications(target);
    // alloc: per toolchain made, a few names
    let lacks: Vec<&str> = told.split(',').filter_map(|it| it.strip_prefix('-')).collect();
    // alloc: per toolchain made, a few names
    let named: Vec<String> = on.iter().cloned().collect();
    for feature in &named {
        let llvm = crate::target::renamed_for_llvm(target.arch, feature, true);
        let own = feature.as_str();
        let llvm = llvm.unwrap_or(std::slice::from_ref(&own));
        if llvm.iter().any(|it| lacks.contains(it)) {
            turn_off(&mut on, implies, feature);
        }
    }
    for feature in host_features(target) {
        turn_on(&mut on, implies, feature);
    }
    Ok((name, on))
}

/// The baseline with every feature the host is found to have of those the code generator
/// picks instructions by, and what each implies: a build's that runs where it is built
/// ([`Runs::Here`]), and `-C target-cpu=native`'s in a blaze without LLVM.
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
        crate::Arch::Wasm32 | crate::Arch::Wasm64 => false,
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
            "sse3",
            "ssse3",
            "sse4.1",
            "sse4.2",
            "popcnt",
            "avx",
            "avx2",
            "fma",
            "f16c",
            "bmi1",
            "bmi2",
            "lzcnt",
            "avx512f",
            "avx512vl",
            "avx512dq",
            "avx512bw",
            "avx512cd",
            "avx512bitalg",
            "avx512vbmi",
            "avx512vnni"
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
        if std::arch::is_aarch64_feature_detected!("paca")
            && std::arch::is_aarch64_feature_detected!("pacg")
        {
            found.extend(["paca", "pacg"]);
        }
    }
    found
}

fn implications(target: &Target) -> &'static [(&'static str, &'static [&'static str])] {
    match target.arch {
        crate::Arch::X86_64 => X86_IMPLIES,
        crate::Arch::Aarch64 => AARCH64_IMPLIES,
        crate::Arch::Wasm32 | crate::Arch::Wasm64 => &[],
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
    "--cfg",
    "--check-cfg",
    "-L",
    "-l",
    "--extern",
    "--cap-lints",
    "-A",
    "-W",
    "-D",
    "-F",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
    "--edition",
    "--crate-type",
    "--crate-name",
    "--emit",
    "--print",
    "-o",
    "--out-dir",
    "--target",
    "--sysroot",
    "--error-format",
    "--json",
    "--color",
    "--explain",
    "--diagnostic-width",
    "--env-set",
    "-Z",
    "--remap-path-scope",
];

/// The `-C` options that are the link's: `blaze_link::args::rustflags` reads each and the link
/// takes or refuses it for the image it makes (docs/cargo.md, "Link arguments"), so a unit that
/// links nothing (a library) is not refused for one, as rustc does not use it there either.
/// `-l` and `-L` are the link's too ([`TAKES_VALUE`]).
const LINK: &[&str] = &[
    "default-linker-libraries",
    "link-arg",
    "link-args",
    "link-dead-code",
    "link-self-contained",
    "linker",
    "linker-flavor",
    "prefer-dynamic",
    "relro-level",
    "rpath",
    "strip",
];

/// The `-C` options that change nothing blaze makes, each with why.
const NOT_CODE: &[&str] = &[
    // rustc's own: deprecated, and does nothing.
    "ar",
    // How many pieces rustc splits a crate into: blaze partitions code its own way, and the
    // program is the same.
    "codegen-units",
    // The program run to make a Windows import library: blaze runs none.
    "dlltool",
    // Whether an rlib carries LLVM bitcode: blaze's rlibs carry no code.
    "embed-bitcode",
    // cargo's file names: blaze names its outputs as cargo's plan says.
    "extra-filename",
    // rustc's incremental directory: blaze keeps its own store.
    "incremental",
    // rustc's own: deprecated, and does nothing.
    "inline-threshold",
    // cargo's crate disambiguator: blaze tells crates apart by their unit.
    "metadata",
    // rustc's own: deprecated, and does nothing.
    "no-stack-check",
    // How debug info is split into files: the profile's `split-debuginfo`, which cargo gives
    // as this flag.
    "split-debuginfo",
    // Lifts rustc's check that crates agree on flags that change the ABI: blaze compiles
    // every crate of a program with one toolchain, so they always agree.
    "unsafe-allow-abi-mismatch",
];

/// The `-C` options that ask for something blaze does not make and are off unless asked:
/// only an off value is taken, and an on one is refused with why.
const OFF_REPORTS: &[(&str, &str)] = &[
    ("remark", "blaze reports no LLVM optimization remarks"),
    ("save-temps", "blaze keeps none of rustc's intermediate files; its own are in its store"),
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
    "control-flow-guard",
    "instrument-coverage",
    "linker-plugin-lto",
    "lto",
    "no-prepopulate-passes",
    "no-redzone",
    "no-vectorize-loops",
    "no-vectorize-slp",
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
                self.cpu =
                    Some(cpu.ok_or_else(|| format!("{}: no CPU named", shown()))?.to_owned());
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
                    // The C runtime linked statically: the link's, not the code's
                    // (`blaze_link::args::rustflags`, which takes or refuses it).
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
                let passes =
                    value.unwrap_or_default().split([' ', ',']).filter(|it| !it.is_empty());
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
            _ if LINK.contains(&name.as_str()) => {}
            _ if NOT_CODE.contains(&name.as_str()) => {}
            _ if OFF.contains(&name.as_str()) && is_no(value) => {}
            // `-C remark=` with no pass named asks for none.
            "remark" if value.is_none_or(str::is_empty) => {}
            "save-temps" if is_no(value) => {}
            "remark" | "save-temps" => {
                let why = OFF_REPORTS.iter().find(|(it, _)| *it == name).map_or("", |it| it.1);
                // alloc: an error
                return Err(format!("{}: {why}", shown()));
            }
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
