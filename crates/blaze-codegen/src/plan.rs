//! How each function of a build is compiled: its backend and settings, and whether its
//! callers reach it through its jump-table slot.
//!
//! - The profile's `opt-level`, with cargo's `[profile.<name>.package.<spec>]` overrides
//!   (`"*"` for every package that is not a workspace member) and `build-override` for what
//!   build scripts and proc macros run, picks the backend (docs/cargo.md, "Backends"): 0 and
//!   1 copy-and-patch stencils, 2, `s` and `z` LLVM at `-O2`, 3 LLVM at `-O3` (#635: tests
//!   are built at the profile's level). Native code is never Cranelift's (AGENTS.md rule 10),
//!   and a target with no stencil library stops at 0 and 1 with an error naming it. wasm32's
//!   one producer is the lowering of Cranelift's IR at every level; Cranelift optimizes
//!   (`opt_level=speed`) all but a workspace member's code there, which no edit reaches and
//!   so is compiled once.
//! - The standard library's own functions are compiled as rustc's shipped std is: `-O3`, no
//!   overflow checks, no debug assertions, line tables. A generic of std is compiled as the
//!   crate that instantiates it asks, as rustc compiles it in that crate's codegen unit.
//! - Only a workspace member's functions are reached through slots in a build that takes
//!   edits: nothing else is ever patched, so its callers call it directly (#1404).
//! - Tiering (#807): a build that takes edits starts a function whose level is LLVM's as
//!   stencils and puts LLVM's code in its slot once compiled. A build whose output is a file
//!   compiles each function once, at its level: the output never depends on timing.
//! - A tiered run ([`Mode::Tiered`]) starts each function at opt-level 0 as stencils that
//!   count, patchable: `blaze_stencil::tier` tiers it up by its counts, not the plan.

use std::{cmp::Ordering, collections::BTreeMap, fmt};

use blaze_hashcons::{Consed, HashCons, Interned};
use blaze_ids::{ContentId, Domain, Hasher};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    Backend,
    settings::{DebugInfo, Panic, Pgo, Sanitize, Spec, Toolchain},
};

pub const POLICY_DOMAIN: &str = "blaze v2 codegen policy 1";

/// cargo's `opt-level`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    S,
    Z,
}

impl OptLevel {
    /// The backend native code at the level is compiled with: copy-and-patch stencils below
    /// LLVM's levels; `None` when it asks for LLVM and blaze has none.
    pub fn backend(self, llvm: bool) -> Option<Backend> {
        match self {
            OptLevel::O0 | OptLevel::O1 => Some(Backend::Stencil),
            OptLevel::O2 | OptLevel::S | OptLevel::Z => llvm.then_some(Backend::Llvm { opt: 2 }),
            OptLevel::O3 => llvm.then_some(Backend::Llvm { opt: 3 }),
        }
    }
}

/// What a profile or one of its overrides sets; `None` leaves what is under it.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct Override {
    pub opt_level: Option<OptLevel>,
    pub overflow_checks: Option<bool>,
    pub debug_assertions: Option<bool>,
    pub debuginfo: Option<DebugInfo>,
}

/// A cargo profile as resolved for one build (`inherits`, the configuration, environment).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Profile {
    pub opt_level: OptLevel,
    pub overflow_checks: bool,
    pub debug_assertions: bool,
    pub debuginfo: DebugInfo,
    pub panic: Panic,
    /// `[profile.<name>.package.<name>]`, by package name, and `"*"`.
    // BTreeMap: `Profile` is serialized and hashed.
    pub packages: BTreeMap<String, Override>,
    /// `[profile.<name>.build-override]`.
    pub build_override: Override,
    /// `codegen-backend = "stencil"` on the profile: every function of the build, the standard
    /// library's and build-time code's too, is copy-and-patch stencils whatever its opt-level,
    /// compiled as at opt-level 1. The build asks for it by name; nothing turns it on for a
    /// blaze that lacks LLVM (rule 3). What the seed builds with (docs/future/bootstrap.md, "std
    /// and assembly in the seed"), whose output only has to be correct. Left out of the
    /// serialized form when off, so [`Policy::id`] of every other build is the one it had.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stencils: bool,
}

/// Whose code an instance is, for its settings. One pointer and a tag: the walk copies it for
/// every callee it keys by who asks for it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// A workspace member's.
    Member(Package),
    /// A dependency's, by package name.
    Dependency(Package),
    /// What a build script or proc macro runs: its own code and its dependencies'.
    BuildTime(Package),
    /// A crate of the standard library's.
    Std,
}

impl Origin {
    /// Its part of a hash: a tag for its kind, then its package's name, length first. What
    /// tells one instance compiled for two origins apart in an ID ([`crate::driver::subject`]).
    pub fn hash_into(&self, hasher: &mut Hasher) {
        let (tag, package) = match self {
            Origin::Member(it) => (0u8, it.as_str()),
            Origin::Dependency(it) => (1, it.as_str()),
            Origin::BuildTime(it) => (2, it.as_str()),
            Origin::Std => (3, ""),
        };
        hasher
            .update(&[tag])
            .update(&(package.len() as u64).to_le_bytes())
            .update(package.as_bytes());
    }
}

/// A package's name, interned once a process: [`Copy`], equal exactly when the names are,
/// hashed by the name's content (the same in every run, never by address), ordered and
/// serialized as the name.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Package(Interned<'static, PackageName>);

#[derive(PartialEq, Eq)]
struct PackageName(Box<str>);

static PACKAGE_NAME: Domain = Domain::new("blaze v2 codegen package name 1");

impl Consed for PackageName {
    fn domain() -> &'static Domain {
        &PACKAGE_NAME
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        hasher.part(self.0.as_bytes());
    }
}

/// Every package name the process has made an [`Origin`] of: the packages of the workspaces
/// it builds, a few thousand at most.
#[expect(
    clippy::disallowed_types,
    reason = "the process's one interner of package names, which every `Origin` points into"
)]
static PACKAGES: std::sync::LazyLock<HashCons<PackageName>> =
    std::sync::LazyLock::new(HashCons::new);

impl Package {
    /// The package named `name`. A name made before is found with no lock and no allocation.
    pub fn new(name: &str) -> Package {
        // `content_of(&PackageName(name))`, without the box.
        let mut hasher = Hasher::of(&PACKAGE_NAME);
        hasher.part(name.as_bytes());
        if let Some(found) = PACKAGES.lookup(&hasher.finish()) {
            return Package(found);
        }
        // alloc: once a process, per package name
        Package(PACKAGES.intern(PackageName(name.into())))
    }

    pub fn as_str(self) -> &'static str {
        &self.0.get().0
    }
}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Package) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Package {
    fn cmp(&self, other: &Package) -> Ordering {
        if self == other { Ordering::Equal } else { self.as_str().cmp(other.as_str()) }
    }
}

impl fmt::Debug for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Package {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Package {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Package, D::Error> {
        let name = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Ok(Package::new(&name))
    }
}

/// What a build makes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mode {
    /// A file: an executable, a test binary, a library. Each function compiled once.
    Output,
    /// A running program that takes edits (`blaze run`, `blaze test --watch`): members'
    /// functions are reached through slots, and what is at LLVM's level starts as stencils.
    Live,
    /// A dev or test run the daemon runs in process (`blaze-inproc`, lazily through
    /// `blaze-lazy`): code at opt-level 0 starts as stencils that count, and
    /// `blaze_stencil::tier` decides each one's tier-up by its counts.
    Tiered,
}

/// How every function of one build is compiled.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Policy {
    pub toolchain: Toolchain,
    pub profile: Profile,
    pub mode: Mode,
    /// #1292: fold functions that differ only in constants. Only where code speed holds:
    /// off for profiles at LLVM's levels unless asked for.
    pub fold: bool,
    /// The build's sanitizers and SanitizerCoverage (`RUSTFLAGS`' `-Zsanitizer` and the
    /// rest), the same for every function.
    pub sanitize: Sanitize,
    /// `-Cprofile-generate` or `-Cprofile-use`, the same for every function.
    pub pgo: Pgo,
    /// The build is of a proc macro's wasm module, or of a crate in it: every function's
    /// [`Spec::proc_macro_bridge`].
    pub proc_macro_bridge: bool,
    /// The program observes no allocation (its global allocator is std's): the
    /// [`Spec::heap_to_stack`] of every function but std's, whose bodies never move one.
    pub heap_to_stack: bool,
    /// Lint `blaze::vecreport` is on: every LLVM unit's [`Spec::report`].
    pub report: bool,
    /// `blaze test --fuel`: the [`Spec::fuel`] of every native function the tests run (not
    /// build-time code's, which runs out of them). Left out of the serialized form when off,
    /// so [`Policy::id`] of every build without it is the one it had before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fuel: bool,
}

/// How one function is compiled.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Choice {
    /// What the build links first.
    pub first: Spec,
    /// What replaces it in its slot once compiled: a tiered function of a live build.
    pub then: Option<Spec>,
    /// Whether its callers reach it through its slot ([`crate::Target::Function`]) or
    /// directly ([`crate::Target::Direct`]).
    pub patchable: bool,
}

/// Why a function can't be compiled as its build asks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// The level asks for LLVM, and this blaze was built without it.
    NoLlvm { origin: Origin, level: OptLevel },
    /// The code is stencils (native code at opt-level 0 or 1, and the first tier of a live
    /// build's or a tiered run's), and the target has no stencil library
    /// ([`Toolchain::stencils`]).
    NoStencils { origin: Origin, level: OptLevel, target: String },
    /// The code must be stencils (the first tier of a live build's or a tiered run's, or every
    /// level of `codegen-backend = "stencil"`), and the toolchain's CPU lacks `off`, a feature
    /// of the target's baseline CPU, whose code the stencils are ([`Toolchain::lacks_baseline`]).
    StencilsNeedBaseline {
        origin: Origin,
        level: OptLevel,
        target: String,
        cpu: String,
        off: String,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoLlvm { origin, level } => write!(
                f,
                "{origin:?} is compiled at opt-level {level:?}, which needs LLVM, and this blaze \
                 was built without it (the seed, without blaze-engine's `native` feature): build \
                 with a blaze that has LLVM, or \
                 ask for stencils at every level with `codegen-backend = \"stencil\"` on the profile"
            ),
            PlanError::NoStencils { origin, level, target } => write!(
                f,
                "{origin:?} is compiled at opt-level {level:?}, whose native code starts as \
                 copy-and-patch stencils, and {target} has no stencil library: native code is \
                 never Cranelift's (AGENTS.md rule 10)"
            ),
            PlanError::StencilsNeedBaseline { origin, level, target, cpu, off } => write!(
                f,
                "{origin:?} is compiled at opt-level {level:?} as copy-and-patch stencils (a live \
                 build's or a tiered run's first tier, or `codegen-backend = \"stencil\"`), which \
                 are the code of {target}'s baseline CPU, and `-C target-cpu={cpu}` has no `{off}` \
                 of it: build this CPU's code without `--live`, tiering or the stencil backend \
                 (LLVM compiles it), or for a CPU that has `{off}`"
            ),
        }
    }
}

/// The standard library as rustc ships it: what its own functions are compiled as.
const SHIPPED_STD: Override = Override {
    opt_level: Some(OptLevel::O3),
    overflow_checks: Some(false),
    debug_assertions: Some(false),
    debuginfo: Some(DebugInfo::LineTablesOnly),
};

impl Policy {
    pub fn id(&self) -> ContentId {
        let bytes = serde_json::to_vec(self).expect("a policy is plain data");
        let mut hasher = Hasher::new(POLICY_DOMAIN);
        hasher.part(&bytes);
        hasher.finish()
    }

    /// The overrides that apply to `origin`, outermost first, as cargo layers them: the
    /// profile, then `"*"` for a non-member, then the package's own; `build-override` over
    /// all of them for build-time code.
    fn layers(&self, origin: &Origin) -> impl Iterator<Item = &Override> {
        let packages = &self.profile.packages;
        let layers = match origin {
            Origin::Member(name) => [packages.get(name.as_str()), None, None],
            Origin::Dependency(name) => [packages.get("*"), packages.get(name.as_str()), None],
            Origin::BuildTime(name) => {
                [packages.get("*"), packages.get(name.as_str()), Some(&self.profile.build_override)]
            }
            Origin::Std => [Some(&SHIPPED_STD), None, None],
        };
        layers.into_iter().flatten()
    }

    /// How a function whose code is `origin`'s is compiled. `overflow_checks` is the front
    /// end's when the instance says (`#[rustc_inherit_overflow_checks]` or std's MIR, built
    /// without them), else the profile's for the origin. A tail call (`become`) changes
    /// nothing here: stencils make one as LLVM does (`return_call`, `musttail`).
    pub fn choose(
        &self,
        origin: &Origin,
        overflow_checks: Option<bool>,
    ) -> Result<Choice, PlanError> {
        let profile = &self.profile;
        let mut level = profile.opt_level;
        let mut checks = profile.overflow_checks;
        let mut assertions = profile.debug_assertions;
        let mut debuginfo = profile.debuginfo;
        for layer in self.layers(origin) {
            level = layer.opt_level.unwrap_or(level);
            checks = layer.overflow_checks.unwrap_or(checks);
            assertions = layer.debug_assertions.unwrap_or(assertions);
            debuginfo = layer.debuginfo.unwrap_or(debuginfo);
        }
        // The profile asked for stencils at every level: a level of LLVM's is compiled as
        // opt-level 1 is, whether or not this blaze has LLVM. Overflow checks, debug
        // assertions and debug info stay the layers' (they are the program's behaviour).
        let asked = level;
        if profile.stencils && level.backend(true) != Some(Backend::Stencil) {
            level = OptLevel::O1;
        }
        let llvm = self.toolchain.llvm.is_some();
        // Only a member's code is edited, so only it is recompiled: the rest is compiled once
        // and can afford Cranelift's optimizer.
        let member = matches!(origin, Origin::Member(_));
        let native = !self.toolchain.arch.is_wasm();
        // wasm32 has one code producer, the lowering of the IR to wasm (`hir-ty-codegen`'s
        // `wasm::lower`, which `check_built` holds every wasm32 unit to): the level picks
        // no other, as LLVM writes no wasm32 for blaze (its pinned build has no WebAssembly
        // target). Native code is stencils or LLVM's, never Cranelift's (AGENTS.md rule 10).
        let backend = match native {
            false => Backend::Cranelift { speed: !member },
            true => level.backend(llvm).ok_or(PlanError::NoLlvm { origin: *origin, level })?,
        };
        // Stencils are the code of the target's baseline CPU: a CPU that lacks one of its
        // features (`-C target-cpu=apple-a7`, which has no LSE) can't run them, and its code at a
        // stencil level is LLVM's at that level, made for its CPU as rustc's is. Which backend
        // is a function of the toolchain, a part of the key, never of a failure.
        let lacks = self.toolchain.lacks_baseline();
        let backend = match (backend, lacks) {
            (Backend::Stencil, Some(_)) if llvm && !profile.stencils => {
                Backend::Llvm { opt: if level == OptLevel::O0 { 0 } else { 1 } }
            }
            (backend, _) => backend,
        };
        let live = self.mode == Mode::Live;
        let spec = Spec {
            // A `Spec` owns its toolchain (an interface across crates); could be an interned ID.
            // clone: once per function planned
            toolchain: self.toolchain.clone(),
            backend,
            overflow_checks: overflow_checks.unwrap_or(checks),
            debug_assertions: assertions,
            panic: profile.panic,
            debuginfo,
            // Folds are Cranelift's (#1292): wasm32's alone.
            fold: self.fold && backend.is_cranelift(),
            counted: false,
            // A member's code built into another function's would outlive a patch of it.
            mir_inline: matches!(level, OptLevel::O2 | OptLevel::O3) && !live,
            sanitize: self.sanitize,
            pgo: self.pgo,
            proc_macro_bridge: self.proc_macro_bridge,
            // Only LLVM's code is read for it; stencil and Cranelift code reports nothing.
            report: self.report && matches!(backend, Backend::Llvm { .. }),
            // std's bodies never move an allocation (`hir-ty-codegen`'s `Build::options`), so
            // their settings leave out a flag that is the program's: one copy of each for every
            // program, whatever its allocator.
            heap_to_stack: self.heap_to_stack && !matches!(origin, Origin::Std),
            // Only code of the tests' processes has the fuel runtime to call; wasm32's one
            // producer has no lowering of it.
            fuel: self.fuel
                && native
                && !self.proc_macro_bridge
                && !matches!(origin, Origin::BuildTime(_)),
        };
        // Build-time code runs out of the run's process, where no counter cells are: it is
        // compiled as an output's.
        let counted = self.mode == Mode::Tiered
            && level == OptLevel::O0
            && !matches!(origin, Origin::BuildTime(_));
        // A live build's LLVM code starts as stencils until LLVM's is in the slot.
        let tiered = live && backend.is_release();
        let stencils = counted || tiered || backend == Backend::Stencil;
        if stencils && self.toolchain.stencils.is_none() {
            // clone: an error names the target
            return Err(PlanError::NoStencils {
                origin: *origin,
                level: asked,
                target: self.toolchain.target.clone(),
            });
        }
        if let (true, Some(off)) = (stencils, lacks) {
            return Err(PlanError::StencilsNeedBaseline {
                origin: *origin,
                level: asked,
                // clone: an error names the target, the CPU and the feature
                target: self.toolchain.target.clone(),
                // clone: an error names the CPU
                cpu: self.toolchain.cpu.clone(),
                // alloc: an error names the feature
                off: off.to_owned(),
            });
        }
        if counted {
            // Its tier-up is `blaze_stencil::tier`'s, by its counts: never a plan's `then`.
            return Ok(Choice {
                first: Spec { fold: false, counted: true, backend: Backend::Stencil, ..spec },
                then: None,
                patchable: true,
            });
        }
        let patchable = live && member;
        Ok(if tiered {
            // The first tier runs only until LLVM's code is in the slot: compiled fast.
            Choice {
                first: Spec { fold: false, report: false, ..spec.at(Backend::Stencil) },
                then: Some(spec),
                patchable,
            }
        } else {
            Choice { first: spec, then: None, patchable }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, settings::Relocation};

    fn policy(level: OptLevel, mode: Mode) -> Policy {
        Policy {
            toolchain: Toolchain {
                id: ContentId([1; 32]),
                target: "aarch64-unknown-linux-gnu".to_owned(),
                arch: Arch::Aarch64,
                cpu: "neoverse-v2".to_owned(),
                features: Vec::new(),
                cranelift: "0.136".to_owned(),
                llvm: Some("23.1".to_owned()),
                stencils: Some("ab".repeat(32)),
                frame_pointers: true,
                relocation: Relocation::Pic,
            },
            profile: Profile {
                opt_level: level,
                overflow_checks: true,
                debug_assertions: true,
                debuginfo: DebugInfo::Full,
                panic: Panic::Unwind,
                packages: BTreeMap::new(),
                build_override: Override::default(),
                stencils: false,
            },
            mode,
            fold: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
            proc_macro_bridge: false,
            report: false,
            heap_to_stack: false,
            fuel: false,
        }
    }

    /// The policy of a proc macro's wasm module: wasm32, whose one producer is Cranelift's IR.
    fn wasm(level: OptLevel) -> Policy {
        let mut policy = policy(level, Mode::Output);
        policy.toolchain.target = "wasm32-wasip1".to_owned();
        policy.toolchain.arch = Arch::Wasm32;
        policy.toolchain.cpu = "generic".to_owned();
        policy.toolchain.llvm = None;
        policy.toolchain.stencils = None;
        policy.proc_macro_bridge = true;
        policy
    }

    fn member() -> Origin {
        Origin::Member(Package::new("app"))
    }

    fn origins() -> [Origin; 4] {
        [
            member(),
            Origin::Dependency(Package::new("regex")),
            Origin::BuildTime(Package::new("serde_derive")),
            Origin::Std,
        ]
    }

    #[test]
    fn dev_is_stencils_and_only_members_are_patched() {
        let policy = policy(OptLevel::O0, Mode::Live);
        let own = policy.choose(&member(), None).unwrap();
        assert_eq!(own.first.backend, Backend::Stencil);
        assert!(!own.first.counted, "a live build's code counts nothing");
        assert!(own.patchable && own.then.is_none());
        let dependency = policy.choose(&Origin::Dependency(Package::new("regex")), None).unwrap();
        assert!(!dependency.patchable);
    }

    /// AGENTS.md rule 10: native code is stencils or LLVM's at every level, origin and mode,
    /// the first tier of a live build's included.
    #[test]
    fn native_code_is_never_cranelifts() {
        let levels =
            [OptLevel::O0, OptLevel::O1, OptLevel::O2, OptLevel::O3, OptLevel::S, OptLevel::Z];
        for level in levels {
            for mode in [Mode::Output, Mode::Live, Mode::Tiered] {
                let mut policy = policy(level, mode);
                policy.fold = true;
                for origin in origins() {
                    let choice = policy.choose(&origin, None).unwrap();
                    for spec in std::iter::once(&choice.first).chain(&choice.then) {
                        assert!(
                            !spec.backend.is_cranelift(),
                            "{level:?} {mode:?} {origin:?}: {:?}",
                            spec.backend
                        );
                        assert!(!spec.fold, "{level:?} {mode:?} {origin:?}: folds are Cranelift's");
                    }
                }
            }
        }
    }

    #[test]
    fn a_target_with_no_stencil_library_stops_below_llvms_levels() {
        for (level, mode) in
            [(OptLevel::O0, Mode::Output), (OptLevel::O1, Mode::Output), (OptLevel::O3, Mode::Live)]
        {
            let mut policy = policy(level, mode);
            policy.toolchain.stencils = None;
            let why = policy.choose(&member(), None).unwrap_err();
            assert_eq!(
                why,
                PlanError::NoStencils {
                    origin: member(),
                    level,
                    target: "aarch64-unknown-linux-gnu".to_owned()
                }
            );
            let message = why.to_string();
            assert!(
                message.contains("Member(\"app\")")
                    && message.contains("aarch64-unknown-linux-gnu"),
                "{message}"
            );
        }
        let mut output = policy(OptLevel::O3, Mode::Output);
        output.toolchain.stencils = None;
        assert_eq!(output.choose(&member(), None).unwrap().first.backend, Backend::Llvm { opt: 3 });
    }

    #[test]
    fn only_a_members_wasm_code_is_unoptimized() {
        for level in [OptLevel::O0, OptLevel::O3] {
            let policy = wasm(level);
            let backend = |origin: Origin| policy.choose(&origin, None).unwrap().first.backend;
            assert_eq!(backend(member()), Backend::Cranelift { speed: false });
            assert_eq!(
                backend(Origin::Dependency(Package::new("regex"))),
                Backend::Cranelift { speed: true }
            );
            assert_eq!(
                backend(Origin::BuildTime(Package::new("serde_derive"))),
                Backend::Cranelift { speed: true }
            );
        }
        let mut folding = wasm(OptLevel::O0);
        folding.fold = true;
        assert!(folding.choose(&member(), None).unwrap().first.fold);
    }

    /// A CPU that lacks a feature of the baseline CPU the stencils are made for runs no
    /// stencil: its code at a stencil level is LLVM's at that level, and code that must be
    /// stencils (a live build's first tier) is an error naming the CPU and the feature.
    #[test]
    fn a_cpu_without_the_baselines_features_is_llvms_below_its_levels() {
        let lacking = |level, mode| {
            let mut policy = policy(level, mode);
            policy.toolchain.cpu = "cortex-a35".to_owned();
            policy.toolchain.features = vec!["-lse".to_owned()];
            policy
        };
        let o0 = lacking(OptLevel::O0, Mode::Output).choose(&member(), None).unwrap();
        assert_eq!(o0.first.backend, Backend::Llvm { opt: 0 });
        let o1 = lacking(OptLevel::O1, Mode::Output).choose(&member(), None).unwrap();
        assert_eq!(o1.first.backend, Backend::Llvm { opt: 1 });
        let live = lacking(OptLevel::O0, Mode::Live).choose(&member(), None);
        let live = live.unwrap_err().to_string();
        assert!(live.contains("cortex-a35") && live.contains("`lse`"), "{live}");
        let plus = policy(OptLevel::O0, Mode::Output);
        assert_eq!(plus.choose(&member(), None).unwrap().first.backend, Backend::Stencil);
    }

    #[test]
    fn std_is_compiled_as_rustc_ships_it() {
        let std = policy(OptLevel::O0, Mode::Output).choose(&Origin::Std, None).unwrap();
        assert_eq!(std.first.backend, Backend::Llvm { opt: 3 });
        assert!(!std.first.overflow_checks && !std.first.debug_assertions);
        assert_eq!(std.first.debuginfo, DebugInfo::LineTablesOnly);
    }

    #[test]
    fn package_overrides_are_layered_as_cargo_layers_them() {
        // `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3` (#635).
        let mut policy = policy(OptLevel::O1, Mode::Output);
        policy.profile.packages.insert(
            "*".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        policy.profile.packages.insert(
            "slow".to_owned(),
            Override { opt_level: Some(OptLevel::O0), ..Override::default() },
        );
        let own = policy.choose(&member(), None).unwrap();
        assert_eq!(own.first.backend, Backend::Stencil);
        let dependency =
            policy.choose(&Origin::Dependency(Package::new("nalgebra")), None).unwrap();
        assert_eq!(dependency.first.backend, Backend::Llvm { opt: 3 });
        let named = policy.choose(&Origin::Dependency(Package::new("slow")), None).unwrap();
        assert_eq!(named.first.backend, Backend::Stencil);
        // `"*"` is not for members, even one named like a dependency.
        let member_star = policy.choose(&Origin::Member(Package::new("nalgebra")), None).unwrap();
        assert_eq!(member_star.first.backend, Backend::Stencil);
    }

    #[test]
    fn build_override_is_outermost_for_build_time_code() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.profile.build_override.opt_level = Some(OptLevel::O0);
        let build = policy.choose(&Origin::BuildTime(Package::new("serde_derive")), None).unwrap();
        assert_eq!(build.first.backend, Backend::Stencil);
    }

    #[test]
    fn a_live_build_tiers_and_an_output_does_not() {
        let mut live = policy(OptLevel::O0, Mode::Live);
        live.fold = true;
        live.profile.packages.insert(
            "*".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        let dependency = Origin::Dependency(Package::new("memchr"));
        let tiered = live.choose(&dependency, None).unwrap();
        assert_eq!(tiered.first.backend, Backend::Stencil);
        assert!(!tiered.first.counted && !tiered.first.fold);
        assert_eq!(tiered.then.as_ref().unwrap().backend, Backend::Llvm { opt: 3 });
        assert_ne!(tiered.first.settings(), tiered.then.unwrap().settings());
        let mut output = live.clone();
        output.mode = Mode::Output;
        let once = output.choose(&dependency, None).unwrap();
        assert_eq!(once.first.backend, Backend::Llvm { opt: 3 });
        assert!(once.then.is_none());
    }

    #[test]
    fn mir_is_inlined_at_two_and_three_outside_live_builds() {
        let inlines = |level, mode| {
            let choice = policy(level, mode).choose(&member(), None).unwrap();
            let last = choice.then.unwrap_or(choice.first);
            last.mir_inline
        };
        for level in [OptLevel::O0, OptLevel::O1, OptLevel::S, OptLevel::Z] {
            assert!(!inlines(level, Mode::Output), "{level:?}");
        }
        for level in [OptLevel::O2, OptLevel::O3] {
            assert!(inlines(level, Mode::Output), "{level:?}");
            assert!(!inlines(level, Mode::Live), "{level:?}");
        }
    }

    #[test]
    fn a_tiered_run_starts_level_zero_as_counting_stencils() {
        let mut tiered = policy(OptLevel::O0, Mode::Tiered);
        tiered.toolchain.stencils = Some("ab".repeat(32));
        tiered.fold = true;
        tiered.profile.packages.insert(
            "nalgebra".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        for origin in [member(), Origin::Dependency(Package::new("regex"))] {
            let choice = tiered.choose(&origin, None).unwrap();
            assert_eq!(choice.first.backend, Backend::Stencil);
            assert!(choice.first.counted && !choice.first.fold && !choice.first.mir_inline);
            assert!(choice.then.is_none() && choice.patchable);
        }
        let mut output = tiered.clone();
        output.mode = Mode::Output;
        let higher = Origin::Dependency(Package::new("nalgebra"));
        let (at_three, as_output) =
            (tiered.choose(&higher, None).unwrap(), output.choose(&higher, None).unwrap());
        assert_eq!(at_three.first, as_output.first);
        assert_eq!((at_three.then, at_three.patchable), (as_output.then, as_output.patchable));
        assert_eq!(
            tiered.choose(&Origin::Std, None).unwrap().first.backend,
            Backend::Llvm { opt: 3 }
        );
        let build = tiered.choose(&Origin::BuildTime(Package::new("serde_derive")), None).unwrap();
        assert_eq!(build.first.backend, Backend::Stencil);
        assert!(!build.first.counted);

        tiered.toolchain.stencils = None;
        assert_eq!(
            tiered.choose(&member(), None),
            Err(PlanError::NoStencils {
                origin: member(),
                level: OptLevel::O0,
                target: "aarch64-unknown-linux-gnu".to_owned()
            })
        );
        assert!(tiered.choose(&higher, None).is_ok());
    }

    #[test]
    fn without_llvm_a_level_that_needs_it_is_refused() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.toolchain.llvm = None;
        assert!(matches!(policy.choose(&member(), None), Err(PlanError::NoLlvm { .. })));
    }

    /// `codegen-backend = "stencil"` (the seed's builds): every level, std's O3 among them, is
    /// stencils as at opt-level 1, with or without LLVM, and the program's settings stay.
    #[test]
    fn a_profile_that_asks_for_stencils_compiles_every_level_as_stencils() {
        for llvm in [Some("23.1".to_owned()), None] {
            let mut policy = policy(OptLevel::O3, Mode::Output);
            policy.toolchain.llvm = llvm;
            policy.profile.stencils = true;
            policy.profile.packages.insert(
                "*".to_owned(),
                Override { opt_level: Some(OptLevel::Z), ..Override::default() },
            );
            for origin in origins() {
                let choice = policy.choose(&origin, None).unwrap();
                assert_eq!(choice.first.backend, Backend::Stencil, "{origin:?}");
                assert!(!choice.first.mir_inline && choice.then.is_none(), "{origin:?}");
            }
            let std = policy.choose(&Origin::Std, None).unwrap().first;
            assert!(!std.overflow_checks && !std.debug_assertions);
            assert!(policy.choose(&member(), None).unwrap().first.overflow_checks);
            policy.toolchain.stencils = None;
            assert_eq!(
                policy.choose(&Origin::Std, None),
                Err(PlanError::NoStencils {
                    origin: Origin::Std,
                    level: OptLevel::O3,
                    target: "aarch64-unknown-linux-gnu".to_owned()
                })
            );
        }
        // Not asked for, a blaze without LLVM still refuses, naming the setting.
        let mut policy = policy(OptLevel::O0, Mode::Output);
        policy.toolchain.llvm = None;
        let why = policy.choose(&Origin::Std, None).unwrap_err().to_string();
        assert!(why.contains("codegen-backend = \"stencil\""), "{why}");
    }

    #[test]
    fn the_front_ends_overflow_checks_win() {
        let policy = policy(OptLevel::O0, Mode::Output);
        let inherited = policy.choose(&Origin::Std, Some(true)).unwrap();
        assert!(inherited.first.overflow_checks);
        let built_without = policy.choose(&member(), Some(false)).unwrap();
        assert!(!built_without.first.overflow_checks);
    }

    #[test]
    fn an_override_of_another_package_changes_no_setting() {
        let a = policy(OptLevel::O0, Mode::Output);
        let mut b = a.clone();
        b.profile.packages.insert(
            "x".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        let syn = Origin::Dependency(Package::new("syn"));
        let settings = |policy: &Policy, origin: &Origin| {
            policy.choose(origin, None).unwrap().first.settings()
        };
        assert_eq!(settings(&a, &member()), settings(&b, &member()));
        assert_eq!(settings(&a, &syn), settings(&b, &syn));
        let mut c = a.clone();
        c.profile.packages.insert(
            "syn".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        assert_ne!(settings(&a, &syn), settings(&c, &syn));
    }

    /// The program's allocator changes the settings of its own and its dependencies'
    /// functions, never std's: std's bodies move no allocation, so one copy of each serves
    /// every program.
    #[test]
    fn heap_to_stack_keys_every_function_but_stds() {
        let off = policy(OptLevel::O2, Mode::Output);
        let on = Policy { heap_to_stack: true, ..off.clone() };
        let settings = |policy: &Policy, origin: &Origin| {
            policy.choose(origin, None).unwrap().first.settings()
        };
        assert_eq!(settings(&off, &Origin::Std), settings(&on, &Origin::Std));
        assert!(!on.choose(&Origin::Std, None).unwrap().first.heap_to_stack);
        for origin in [member(), Origin::Dependency(Package::new("regex"))] {
            assert_ne!(settings(&off, &origin), settings(&on, &origin), "{origin:?}");
        }
    }

    fn std_hash(value: &impl std::hash::Hash) -> u64 {
        use std::hash::BuildHasher;
        blaze_hash::FixedState.hash_one(value)
    }

    #[test]
    fn a_package_is_its_name() {
        // A name made from another string is the same package: equal, hashed alike.
        let owned = String::from("serde");
        let (a, b) = (Package::new("serde"), Package::new(&owned));
        assert_eq!(a, b);
        assert_eq!(std_hash(&a), std_hash(&b));
        assert_eq!(a.as_str(), "serde");
        assert_ne!(Package::new("serde"), Package::new("serde_json"));
        assert_ne!(Origin::Member(a), Origin::Dependency(a));
        // Ordered by name, whatever order the names were made in.
        let (z, m) = (Package::new("zz-made-first"), Package::new("aa-made-second"));
        assert!(m < z && Origin::Member(m) < Origin::Member(z));
        assert_eq!(format!("{:?}", Origin::Member(a)), "Member(\"serde\")");
    }

    #[test]
    fn fuel_meters_what_the_tests_run_and_leaves_the_rest_as_it_was() {
        let off = policy(OptLevel::O0, Mode::Output);
        let json = serde_json::to_string(&off).unwrap();
        assert!(!json.contains("fuel"), "a policy without fuel serializes as before: {json}");
        let spec = off.choose(&member(), None).unwrap().first;
        assert!(!serde_json::to_string(&spec).unwrap().contains("fuel"));
        let mut on = off.clone();
        on.fuel = true;
        assert_ne!(on.id(), off.id());
        for origin in [member(), Origin::Dependency(Package::new("regex")), Origin::Std] {
            assert!(on.choose(&origin, None).unwrap().first.fuel, "{origin:?}");
        }
        let build = on.choose(&Origin::BuildTime(Package::new("serde_derive")), None).unwrap();
        assert!(!build.first.fuel, "build-time code runs out of the tests");
    }

    #[test]
    fn an_origin_is_serialized_as_its_name() {
        let origin = Origin::BuildTime(Package::new("serde_derive"));
        let json = serde_json::to_string(&origin).unwrap();
        assert_eq!(json, r#"{"BuildTime":"serde_derive"}"#);
        assert_eq!(serde_json::from_str::<Origin>(&json).unwrap(), origin);
    }

    #[test]
    fn copying_and_finding_a_package_allocates_nothing() {
        let origin = Origin::Dependency(Package::new("regex"));
        let mut copies = 0;
        blaze_alloc_count::assert_at_most(0, "a package made again, and its origin copied", || {
            for _ in 0..64 {
                let again = Origin::Dependency(Package::new("regex"));
                copies += usize::from(again == origin);
            }
        });
        assert_eq!(copies, 64);
    }
}
