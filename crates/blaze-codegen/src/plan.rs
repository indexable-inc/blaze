//! How each function of a build is compiled: its backend and settings, and whether its
//! callers reach it through its jump-table slot.
//!
//! - The profile's `opt-level`, with cargo's `[profile.<name>.package.<spec>]` overrides
//!   (`"*"` for every package that is not a workspace member) and `build-override` for what
//!   build scripts and proc macros run, picks the backend (docs/cargo.md, "Backends"): 0 and
//!   1 Cranelift, 2, `s` and `z` LLVM at `-O2`, 3 LLVM at `-O3` (#635: tests are built at the
//!   profile's level). Cranelift optimizes (`opt_level=speed`) all but a workspace member's
//!   code, which no edit reaches and so is compiled once.
//! - The standard library's own functions are compiled as rustc's shipped std is: `-O3`, no
//!   overflow checks, no debug assertions, line tables. A generic of std is compiled as the
//!   crate that instantiates it asks, as rustc compiles it in that crate's codegen unit.
//! - Only a workspace member's functions are reached through slots in a build that takes
//!   edits: nothing else is ever patched, so its callers call it directly (#1404).
//! - Tiering (#807): a build that takes edits starts a function whose level is LLVM's at
//!   Cranelift and puts LLVM's code in its slot once compiled. A build whose output is a file
//!   compiles each function once, at its level: the output never depends on timing.
//! - A tiered run ([`Mode::Tiered`]) starts each function at opt-level 0 as stencils that
//!   count, patchable: `blaze_stencil::tier` tiers it up by its counts, not the plan.

use std::{cmp::Ordering, collections::BTreeMap, fmt, sync::LazyLock};

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
    /// The backend the level is compiled with, Cranelift at `opt_level=none`; `None` when it
    /// asks for LLVM and blaze has none.
    pub fn backend(self, llvm: bool) -> Option<Backend> {
        match self {
            OptLevel::O0 | OptLevel::O1 => Some(Backend::Cranelift { speed: false }),
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
static PACKAGES: LazyLock<HashCons<PackageName>> = LazyLock::new(HashCons::new);

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
    /// functions are reached through slots, and what is at LLVM's level starts at
    /// Cranelift's.
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
    /// A tiered build's code at opt-level 0 is stencils, and the target has no stencil
    /// library ([`Toolchain::stencils`]).
    NoStencils { target: String },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoLlvm { origin, level } => write!(
                f,
                "{origin:?} is compiled at opt-level {level:?}, which needs LLVM, and this blaze \
                 was built without it (`--features llvm`)"
            ),
            PlanError::NoStencils { target } => write!(
                f,
                "a tiered run compiles opt-level 0 code as stencils, and {target} has no stencil \
                 library"
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
    /// without them), else the profile's for the origin.
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
        let llvm = self.toolchain.llvm.is_some();
        let backend =
            level.backend(llvm).ok_or(PlanError::NoLlvm { origin: *origin, level })?;
        // Only a member's code is edited, so only it is recompiled: the rest is compiled once
        // and can afford Cranelift's optimizer.
        let member = matches!(origin, Origin::Member(_));
        let backend = match backend {
            Backend::Cranelift { .. } => Backend::Cranelift { speed: !member },
            other => other,
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
            fold: self.fold && backend.is_cranelift(),
            counted: false,
            // A member's code built into another function's would outlive a patch of it.
            mir_inline: matches!(level, OptLevel::O2 | OptLevel::O3) && !live,
            sanitize: self.sanitize,
            pgo: self.pgo,
        };
        // Build-time code runs out of the run's process, where no counter cells are: it is
        // compiled as an output's.
        let counted = self.mode == Mode::Tiered && level == OptLevel::O0 && !matches!(origin, Origin::BuildTime(_));
        if counted {
            if self.toolchain.stencils.is_none() {
                // clone: an error names the target
                return Err(PlanError::NoStencils { target: self.toolchain.target.clone() });
            }
            // Its tier-up is `blaze_stencil::tier`'s, by its counts: never a plan's `then`.
            return Ok(Choice {
                first: Spec { fold: false, counted: true, backend: Backend::Stencil, ..spec },
                then: None,
                patchable: true,
            });
        }
        let patchable = live && member;
        let tiered = live && !backend.is_cranelift();
        Ok(if tiered {
            // The first tier runs only until LLVM's code is in the slot: compiled fast.
            Choice {
                first: Spec { fold: self.fold, ..spec.at(Backend::Cranelift { speed: false }) },
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
                stencils: None,
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
            },
            mode,
            fold: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
        }
    }

    fn member() -> Origin {
        Origin::Member(Package::new("app"))
    }

    #[test]
    fn dev_is_cranelift_and_only_members_are_patched() {
        let policy = policy(OptLevel::O0, Mode::Live);
        let own = policy.choose(&member(), None).unwrap();
        assert_eq!(own.first.backend, Backend::Cranelift { speed: false });
        assert!(own.patchable && own.then.is_none());
        let dependency = policy.choose(&Origin::Dependency(Package::new("regex")), None).unwrap();
        assert!(!dependency.patchable);
    }

    #[test]
    fn only_a_members_cranelift_code_is_unoptimized() {
        for mode in [Mode::Output, Mode::Live] {
            let policy = policy(OptLevel::O0, mode);
            let speed = |origin: Origin| policy.choose(&origin, None).unwrap().first.backend;
            assert_eq!(speed(member()), Backend::Cranelift { speed: false });
            assert_eq!(speed(Origin::Dependency(Package::new("regex"))), Backend::Cranelift { speed: true });
            assert_eq!(speed(Origin::BuildTime(Package::new("serde_derive"))), Backend::Cranelift { speed: true });
        }
        let dependency = policy(OptLevel::O0, Mode::Output).choose(&Origin::Dependency(Package::new("regex")), None);
        let own = policy(OptLevel::O0, Mode::Output).choose(&member(), None);
        assert_ne!(dependency.unwrap().first.settings(), own.unwrap().first.settings());
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
        assert_eq!(own.first.backend, Backend::Cranelift { speed: false });
        let dependency = policy.choose(&Origin::Dependency(Package::new("nalgebra")), None).unwrap();
        assert_eq!(dependency.first.backend, Backend::Llvm { opt: 3 });
        let named = policy.choose(&Origin::Dependency(Package::new("slow")), None).unwrap();
        assert_eq!(named.first.backend, Backend::Cranelift { speed: true });
        // `"*"` is not for members, even one named like a dependency.
        let member_star = policy.choose(&Origin::Member(Package::new("nalgebra")), None).unwrap();
        assert_eq!(member_star.first.backend, Backend::Cranelift { speed: false });
    }

    #[test]
    fn build_override_is_outermost_for_build_time_code() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.profile.build_override.opt_level = Some(OptLevel::O0);
        let build = policy.choose(&Origin::BuildTime(Package::new("serde_derive")), None).unwrap();
        assert_eq!(build.first.backend, Backend::Cranelift { speed: true });
    }

    #[test]
    fn a_live_build_tiers_and_an_output_does_not() {
        let mut live = policy(OptLevel::O0, Mode::Live);
        live.profile.packages.insert(
            "*".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        let dependency = Origin::Dependency(Package::new("memchr"));
        let tiered = live.choose(&dependency, None).unwrap();
        assert_eq!(tiered.first.backend, Backend::Cranelift { speed: false });
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
        let (at_three, as_output) = (tiered.choose(&higher, None).unwrap(), output.choose(&higher, None).unwrap());
        assert_eq!(at_three.first, as_output.first);
        assert_eq!((at_three.then, at_three.patchable), (as_output.then, as_output.patchable));
        assert_eq!(tiered.choose(&Origin::Std, None).unwrap().first.backend, Backend::Llvm { opt: 3 });
        let build = tiered.choose(&Origin::BuildTime(Package::new("serde_derive")), None).unwrap();
        assert_eq!(build.first.backend, Backend::Cranelift { speed: true });
        assert!(!build.first.counted);

        tiered.toolchain.stencils = None;
        assert_eq!(
            tiered.choose(&member(), None),
            Err(PlanError::NoStencils { target: "aarch64-unknown-linux-gnu".to_owned() })
        );
        assert!(tiered.choose(&higher, None).is_ok());
    }

    #[test]
    fn without_llvm_a_level_that_needs_it_is_refused() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.toolchain.llvm = None;
        assert!(matches!(policy.choose(&member(), None), Err(PlanError::NoLlvm { .. })));
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
        let settings = |policy: &Policy, origin: &Origin| policy.choose(origin, None).unwrap().first.settings();
        assert_eq!(settings(&a, &member()), settings(&b, &member()));
        assert_eq!(settings(&a, &syn), settings(&b, &syn));
        let mut c = a.clone();
        c.profile.packages.insert(
            "syn".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        assert_ne!(settings(&a, &syn), settings(&c, &syn));
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
