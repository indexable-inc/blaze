//! cargo's profile as blaze's [`Profile`]: the one home of the derivation every builder of a
//! [`super::Policy`] takes (the daemon's `serve/unit/codegen.rs` `policy`, the no-daemon
//! command's `graph.rs` `program_of`). Sans-IO: it reads only the values it is handed, and its
//! refusals borrow them ([`ProfileRefused`]), each naming the value and the rule.
//!
//! What stays the caller's: which profiles are linked in (the root's own first, std's left
//! out), the goal a request asked (and the build-time rule that build scripts and proc macros
//! keep their profile's own), the rest of the policy (toolchain, mode, sanitizers, profile
//! data read from disk).

use std::{collections::BTreeMap, fmt};

/// The goal a profile is made for, re-exported so a caller needs no other crate to name it.
pub use blaze_api::Goal;

use super::{OptLevel, Override, Profile};
use crate::{
    Backend,
    settings::{DebugInfo, Panic},
};

/// cargo's `debug` setting as its unit plan writes it: a level or a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CargoDebug<'a> {
    Level(u8),
    Named(&'a str),
}

/// One unit's cargo profile, as resolved for its build (`inherits`, configuration,
/// environment), borrowed from the unit plan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CargoProfile<'a> {
    /// The profile's name (`dev`, `release`).
    pub name: &'a str,
    /// The package the unit is of, by name: what `[profile.*.package.*]` keys by.
    pub package: &'a str,
    pub opt_level: &'a str,
    pub codegen_backend: Option<&'a str>,
    pub debuginfo: CargoDebug<'a>,
    pub panic: &'a str,
    pub overflow_checks: bool,
    pub debug_assertions: bool,
}

/// What a profile asks that blaze does not build, borrowing the value asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProfileRefused<'a> {
    OptLevel(&'a str),
    DebugInfo(CargoDebug<'a>),
    Panic(&'a str),
    /// `codegen-backend = "cranelift"` at a level blaze compiles with LLVM; `goal` is the goal
    /// and the level when the goal moved the profile's own level there.
    Cranelift { profile: &'a str, package: &'a str, opt_level: &'a str, goal: Option<(Goal, OptLevel)> },
    UnknownBackend { profile: &'a str, package: &'a str, asked: &'a str },
    /// A linked package's backend that is not the build's: `stencil` is the whole build's.
    MixedBackend { profile: &'a str, package: &'a str, asked: &'a str, build: &'a str },
    /// `--goal max` of a profile that asks for stencils.
    MaxWithStencil { profile: &'a str, package: &'a str },
    /// `-C profile-generate`: the flag as given.
    ProfileGenerate(&'a str),
    /// `-C profile-use` naming no file: the flag as given.
    ProfileUseNoPath(&'a str),
}

impl ProfileRefused<'_> {
    /// The rule refused by, in a few words: what a caller that keeps rule constants names.
    pub fn rule(&self) -> &'static str {
        match self {
            ProfileRefused::OptLevel(_) | ProfileRefused::DebugInfo(_) | ProfileRefused::Panic(_) => {
                "a profile value is one rustc takes"
            }
            ProfileRefused::Cranelift { .. }
            | ProfileRefused::UnknownBackend { .. }
            | ProfileRefused::MixedBackend { .. }
            | ProfileRefused::MaxWithStencil { .. } => "a codegen backend is one blaze compiles with, for the whole build",
            ProfileRefused::ProfileGenerate(_) | ProfileRefused::ProfileUseNoPath(_) => {
                "profile-guided optimization is asked as blaze does it"
            }
        }
    }
}

impl fmt::Display for ProfileRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProfileRefused::OptLevel(it) => write!(f, "opt-level `{it}` is none rustc takes"),
            ProfileRefused::DebugInfo(CargoDebug::Level(it)) => write!(f, "debuginfo `{it}` is none rustc takes"),
            ProfileRefused::DebugInfo(CargoDebug::Named(it)) => write!(f, "debuginfo `{it}` is none rustc takes"),
            ProfileRefused::Panic(it) => write!(f, "panic strategy `{it}` is none rustc takes"),
            ProfileRefused::Cranelift { profile, package, opt_level, goal } => {
                write!(
                    f,
                    "the profile `{profile}` of `{package}` asks for codegen-backend `cranelift` at opt-level \
                     {opt_level}, which blaze compiles with LLVM: native code is never Cranelift's (AGENTS.md rule \
                     10); `stencil` compiles every level as copy-and-patch stencils"
                )?;
                match goal {
                    Some((goal, at)) => write!(f, " (the goal `{goal}` compiles the profile's own code at {at:?})"),
                    None => Ok(()),
                }
            }
            ProfileRefused::UnknownBackend { profile, package, asked } => write!(
                f,
                "the profile `{profile}` of `{package}` asks for codegen-backend `{asked}`, which blaze has \
                 none of (`cranelift`, `stencil`, `llvm`)"
            ),
            ProfileRefused::MixedBackend { profile, package, asked, build } => write!(
                f,
                "the profile `{profile}` of `{package}` asks for codegen-backend `{asked}`, and the build's for \
                 `{build}`: blaze takes `stencil` for a whole build, from the profile, never per package"
            ),
            ProfileRefused::MaxWithStencil { profile, package } => write!(
                f,
                "`--goal max` waits for the optimized code of `{package}`, and its profile `{profile}` asks for \
                 codegen-backend `stencil`, which compiles every function as tier 0 code: `max` is never \
                 answered with tier 0 code (docs/v2/plan/design/tiers.md); drop `--goal max`, or the \
                 profile's `codegen-backend`"
            ),
            ProfileRefused::ProfileGenerate(option) => write!(
                f,
                "the flags for rustc have `-C {option}`; blaze's linker writes no profile sections yet \
                 (`__llvm_prf_data`, `__llvm_prf_cnts`, `__llvm_prf_names` and the profile runtime), so an \
                 instrumented binary would write no profile, and blaze does not build with it"
            ),
            ProfileRefused::ProfileUseNoPath(option) => write!(
                f,
                "the flags for rustc have `-C {option}`, which names no profile: it takes \
                 `-C profile-use=PATH`, PATH a `.profdata`"
            ),
        }
    }
}

/// cargo's `opt-level`.
pub fn opt_level(level: &str) -> Result<OptLevel, ProfileRefused<'_>> {
    Ok(match level {
        "0" => OptLevel::O0,
        "1" => OptLevel::O1,
        "2" => OptLevel::O2,
        "3" => OptLevel::O3,
        "s" => OptLevel::S,
        "z" => OptLevel::Z,
        other => return Err(ProfileRefused::OptLevel(other)),
    })
}

/// cargo's `debug`.
pub fn debuginfo(debug: CargoDebug<'_>) -> Result<DebugInfo, ProfileRefused<'_>> {
    Ok(match debug {
        CargoDebug::Level(0) => DebugInfo::None,
        CargoDebug::Level(1) => DebugInfo::Limited,
        CargoDebug::Level(2) => DebugInfo::Full,
        CargoDebug::Named("none" | "false") => DebugInfo::None,
        CargoDebug::Named("line-tables-only" | "line-directives-only") => DebugInfo::LineTablesOnly,
        CargoDebug::Named("limited") => DebugInfo::Limited,
        CargoDebug::Named("full" | "true") => DebugInfo::Full,
        other => return Err(ProfileRefused::DebugInfo(other)),
    })
}

/// cargo's `panic`.
pub fn panic(panic: &str) -> Result<Panic, ProfileRefused<'_>> {
    match panic {
        "unwind" => Ok(Panic::Unwind),
        "abort" => Ok(Panic::Abort),
        other => Err(ProfileRefused::Panic(other)),
    }
}

/// What blaze makes of `profile`'s `codegen-backend` for its code at `level`: `true` when it
/// asks for stencils at every level ([`Profile::stencils`]). `llvm`, rustc's default, leaves
/// the machine code blaze's choice, and so does `cranelift` where the level compiles with
/// stencils; any other is refused by name, never a key ignored.
pub fn stencils<'a>(profile: &CargoProfile<'a>, level: OptLevel) -> Result<bool, ProfileRefused<'a>> {
    match profile.codegen_backend {
        None | Some("llvm") => Ok(false),
        Some("stencil") => Ok(true),
        Some("cranelift") if level.backend(false) == Some(Backend::Stencil) => Ok(false),
        Some("cranelift") => Err(ProfileRefused::Cranelift {
            profile: profile.name,
            package: profile.package,
            opt_level: profile.opt_level,
            goal: None,
        }),
        Some(asked) => Err(ProfileRefused::UnknownBackend { profile: profile.name, package: profile.package, asked }),
    }
}

/// The profile a build compiles every function under: `base`'s, the root unit's, at the tier
/// `goal` picks (docs/v2/plan/design/tiers.md: [`OptLevel::tier_zero`] under `edit`,
/// [`OptLevel::optimized`] under `max`; a profile at its goal's tier keeps its own level, so
/// its policy and every key made of it are what they were before goals), with an
/// [`Override`] for each package of `linked` (the other packages linked in, std's left out)
/// whose profile differs from `base` (cargo's `[profile.*.package.*]`; such a package keeps
/// its level under both goals). `macro_wasm`: a wasm proc macro's module aborts on a panic,
/// as its std was planned (wasm32 code unwinds nowhere).
///
/// `stencil` is the whole build's: a linked package that leaves it, or asks for it where the
/// rest is at LLVM's levels, is refused ([`ProfileRefused::MixedBackend`]), and so is `max` of
/// a profile that asks for it.
///
/// Cost: one pass over `linked`, one `String` per package that differs (the key of
/// [`Profile::packages`], which is serialized and hashed).
pub fn profile_of<'a>(
    base: &CargoProfile<'a>,
    linked: &[CargoProfile<'a>],
    goal: Goal,
    macro_wasm: bool,
) -> Result<Profile, ProfileRefused<'a>> {
    let opt = opt_level(base.opt_level)?;
    if goal == Goal::Max && base.codegen_backend == Some("stencil") {
        return Err(ProfileRefused::MaxWithStencil { profile: base.name, package: base.package });
    }
    let at = match goal {
        Goal::Edit => opt.tier_zero(),
        Goal::Max => opt.optimized(),
    };
    let stencils = stencils(base, at).map_err(|why| match (why, at == opt) {
        (ProfileRefused::Cranelift { profile, package, opt_level, .. }, false) => {
            ProfileRefused::Cranelift { profile, package, opt_level, goal: Some((goal, at)) }
        }
        (why, _) => why,
    })?;
    let info = debuginfo(base.debuginfo)?;
    // BTreeMap: it is `Profile::packages`, which is serialized and hashed.
    let mut packages = BTreeMap::new();
    for dep in linked {
        let level = opt_level(dep.opt_level)?;
        if self::stencils(dep, level)? != stencils && (stencils || level.backend(false) != Some(Backend::Stencil)) {
            return Err(ProfileRefused::MixedBackend {
                profile: dep.name,
                package: dep.package,
                asked: dep.codegen_backend.unwrap_or("llvm"),
                build: base.codegen_backend.unwrap_or("llvm"),
            });
        }
        let dep_info = debuginfo(dep.debuginfo)?;
        let differs = Override {
            opt_level: (level != opt).then_some(level),
            overflow_checks: (dep.overflow_checks != base.overflow_checks).then_some(dep.overflow_checks),
            debug_assertions: (dep.debug_assertions != base.debug_assertions).then_some(dep.debug_assertions),
            debuginfo: (dep_info != info).then_some(dep_info),
        };
        if differs != Override::default() {
            packages.insert(String::from(dep.package), differs);
        }
    }
    Ok(Profile {
        opt_level: at,
        overflow_checks: base.overflow_checks,
        debug_assertions: base.debug_assertions,
        debuginfo: info,
        panic: match macro_wasm {
            true => Panic::Abort,
            false => panic(base.panic)?,
        },
        packages,
        build_override: Override::default(),
        stencils,
    })
}

/// What a build's rustflags ask of profile-guided optimization: to build with the profile at
/// `path`, as the flags give it (`-C profile-use=PATH`). Reading it is the caller's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PgoAsk<'a> {
    pub path: &'a str,
}

/// The profile-guided optimization `rustflags` ask: `None`, or the last `-C profile-use`, as
/// rustc takes a codegen option given twice. One naming no file is refused, and so is
/// `-C profile-generate`: blaze's linker writes no profile sections yet, so an instrumented
/// binary would count into nothing.
///
/// The sanitizer half of what the check lane asked as `instrumented` is not here: it is
/// blaze-cargo's `unstable.rs` (the `-Z` flag table and the sanitizer runtimes), which moves
/// whole in one port.
pub fn pgo_ask<S: AsRef<str>>(rustflags: &[S]) -> Result<Option<PgoAsk<'_>>, ProfileRefused<'_>> {
    let mut asked = None;
    let mut flags = rustflags.iter().map(AsRef::as_ref);
    while let Some(flag) = flags.next() {
        let option = match flag {
            "-C" | "--codegen" => flags.next().unwrap_or(""),
            _ => match flag.strip_prefix("--codegen=").or_else(|| flag.strip_prefix("-C")) {
                Some(option) => option,
                None => continue,
            },
        };
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        match name {
            "profile-generate" | "profile_generate" => return Err(ProfileRefused::ProfileGenerate(option)),
            "profile-use" | "profile_use" => match value {
                Some(path) if !path.is_empty() => asked = Some(PgoAsk { path }),
                _ => return Err(ProfileRefused::ProfileUseNoPath(option)),
            },
            _ => {}
        }
    }
    Ok(asked)
}
