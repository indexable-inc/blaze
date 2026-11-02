//! Everything besides the function that changes its machine code ([`Spec`]), and its content
//! ID ([`crate::Settings`]). A field left out of [`Spec::settings`] is stale code: every
//! field is hashed, and a test changes each one alone.

use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::{Arch, Backend, Settings};

pub const SETTINGS_DOMAIN: &str = "blaze v2 codegen settings 5";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Panic {
    Unwind,
    Abort,
}

/// rustc's `-C debuginfo`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum DebugInfo {
    None,
    LineTablesOnly,
    Limited,
    Full,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Relocation {
    /// Position-independent: what every executable blaze links is.
    Pic,
    Static,
}

/// How the program is built, for a function's settings: the toolchain and target, the same
/// for every function of a build.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: setup, once per plan and per unit's settings
pub struct Toolchain {
    /// The pinned nightly's and blaze's own code identity (`harness/toolchain` and the hash of
    /// the blaze executable, `hir_ty_codegen::v2::build`): another blaze is other code.
    pub id: ContentId,
    /// The target triple.
    pub target: String,
    pub arch: Arch,
    /// `-C target-cpu`, resolved (`native` is the host's name).
    pub cpu: String,
    /// `-C target-feature`, resolved and sorted.
    pub features: Vec<String>,
    /// The backends' versions, as they name themselves.
    pub cranelift: String,
    /// `None` for a blaze built without LLVM: a unit that asks for it is refused.
    pub llvm: Option<String>,
    /// The hex of `blaze_stencil::library_id` for the target: the stencils
    /// [`Backend::Stencil`] copies. `None` for a target with no library (wasm): a unit that
    /// asks for it is refused.
    pub stencils: Option<String>,
    pub frame_pointers: bool,
    pub relocation: Relocation,
}

/// rustc's `-Zsanitizer` (address, thread, memory), `-Zsanitizer-recover`,
/// `-Zsanitizer-memory-track-origins`, and SanitizerCoverage as rustc gets it
/// (`-Cpasses=sancov-module` with `-Cllvm-args=-sanitizer-coverage-*`): one for the whole
/// build, as cargo passes `RUSTFLAGS` to every crate. `Sanitize::default()` is none.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Sanitize {
    pub sanitizers: Sanitizers,
    /// Of `sanitizers`, those that report and go on (`-Zsanitizer-recover`).
    pub recover: Sanitizers,
    /// `-Zsanitizer-memory-track-origins`: 0, 1 or 2; 0 without `Sanitizers::MEMORY`.
    pub track_origins: u8,
    pub coverage: Coverage,
}

impl Sanitize {
    pub const fn is_empty(&self) -> bool {
        self.sanitizers.is_empty() && !self.coverage.is_on()
    }
}

/// A set of sanitizers, as bits.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Sanitizers(pub u8);

impl Sanitizers {
    pub const NONE: Sanitizers = Sanitizers(0);
    pub const ADDRESS: Sanitizers = Sanitizers(1 << 0);
    pub const THREAD: Sanitizers = Sanitizers(1 << 1);
    pub const MEMORY: Sanitizers = Sanitizers(1 << 2);
    /// In a function's set of what is off for it only (`Function::unsanitized`): LLVM's
    /// SanitizerCoverage skips it, as it skips a function named `__sanitizer_*`. Never in
    /// `Sanitize::sanitizers` or `recover`.
    pub const COVERAGE: Sanitizers = Sanitizers(1 << 3);

    pub const fn contains(self, other: Sanitizers) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn union(self, other: Sanitizers) -> Sanitizers {
        Sanitizers(self.0 | other.0)
    }
    pub const fn without(self, other: Sanitizers) -> Sanitizers {
        Sanitizers(self.0 & !other.0)
    }
}

/// SanitizerCoverage: `-sanitizer-coverage-level` (1 the entry block, 2 every block, 3 and
/// critical edges, 4 and indirect calls) and the `-sanitizer-coverage-*` switches as bits.
/// Canonical: `Coverage::default()` (level 0, no flags) unless `-Cpasses` names
/// `sancov-module` and the level is 1 to 4, so two builds without coverage have one key.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Coverage {
    pub level: u8,
    pub flags: u16,
}

impl Coverage {
    pub const TRACE_PC: u16 = 1 << 0;
    pub const TRACE_PC_GUARD: u16 = 1 << 1;
    pub const INLINE_8BIT_COUNTERS: u16 = 1 << 2;
    pub const INLINE_BOOL_FLAG: u16 = 1 << 3;
    pub const PC_TABLE: u16 = 1 << 4;
    pub const TRACE_CMP: u16 = 1 << 5;
    pub const TRACE_DIV: u16 = 1 << 6;
    pub const TRACE_GEP: u16 = 1 << 7;
    pub const TRACE_LOADS: u16 = 1 << 8;
    pub const TRACE_STORES: u16 = 1 << 9;
    pub const NO_PRUNE: u16 = 1 << 10;
    pub const STACK_DEPTH: u16 = 1 << 11;
    pub const TRACE_PC_ENTRY_EXIT: u16 = 1 << 12;
    pub const CONTROL_FLOW: u16 = 1 << 13;

    pub const fn is_on(self) -> bool {
        matches!(self.level, 1..=4)
    }
}

/// Profile-guided optimization in the LLVM back end (rustc's `-Cprofile-generate` and
/// `-Cprofile-use`): the profile keys the code it shaped, so a new profile is a rebuild.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum Pgo {
    #[default]
    None,
    /// LLVM's IR instrumentation, writing raw profiles (`default_%m.profraw`).
    Generate,
    /// The `.profdata` in the store under this content ID.
    Use(ContentId),
}

/// What changes one function's machine code besides its MIR and its type arguments' facts.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spec {
    pub toolchain: Toolchain,
    pub backend: Backend,
    /// `-C overflow-checks` of the crate whose MIR the instance is, as rustc builds it
    /// (`#[rustc_inherit_overflow_checks]` takes the caller's).
    pub overflow_checks: bool,
    /// `-C debug-assertions` of the crate that compiles the instance: what
    /// `intrinsics::ub_checks()` is at codegen.
    pub debug_assertions: bool,
    pub panic: Panic,
    pub debuginfo: DebugInfo,
    /// Folding functions that differ only in constants (#1292): changes which body a
    /// function's code is.
    pub fold: bool,
    /// The code counts its calls and back edges into its own `Target::Counter` cells, for
    /// tier-up (`blaze_stencil::tier`): stencil code always does, the Cranelift tier of a
    /// tiered run does, LLVM code never does.
    pub counted: bool,
    /// Whether rustc's MIR inliner runs on the bodies of the crates compiled at these
    /// settings (`Inline::is_enabled`): `-Copt-level` 2 or 3, not `s`/`z`, which share
    /// `Llvm { opt: 2 }`, and not in a build that patches, as rustc runs it only outside
    /// incremental builds ([`crate::plan::Policy::choose`]). The library's MIR is inlined
    /// whatever this says, as rustc ships it.
    pub mir_inline: bool,
    /// The sanitizers and coverage the LLVM back end instruments with: a sanitized build's
    /// code is never a plain one's.
    pub sanitize: Sanitize,
    pub pgo: Pgo,
}

impl Spec {
    pub fn settings(&self) -> Settings {
        let Spec {
            toolchain,
            backend,
            overflow_checks,
            debug_assertions,
            panic,
            debuginfo,
            fold,
            counted,
            mir_inline,
            sanitize,
            pgo,
        } = self;
        let Toolchain {
            id,
            target,
            arch,
            cpu,
            features,
            cranelift,
            llvm,
            stencils,
            frame_pointers,
            relocation,
        } = toolchain;
        let mut hasher = Hasher::new(SETTINGS_DOMAIN);
        hasher.id(id).part(target.as_bytes());
        hasher.update(&[crate::content::arch_tag(*arch)]).part(cpu.as_bytes());
        hasher.update(&(features.len() as u64).to_le_bytes());
        for feature in features {
            hasher.part(feature.as_bytes());
        }
        hasher.part(cranelift.as_bytes());
        match llvm {
            Some(llvm) => hasher.update(&[1]).part(llvm.as_bytes()),
            None => hasher.update(&[0]),
        };
        match stencils {
            Some(stencils) => hasher.update(&[1]).part(stencils.as_bytes()),
            None => hasher.update(&[0]),
        };
        hasher.update(&[*frame_pointers as u8, *relocation as u8]);
        match backend {
            Backend::Cranelift { speed } => hasher.update(&[0, u8::from(*speed)]),
            Backend::Llvm { opt } => hasher.update(&[1, *opt]),
            Backend::Stencil => hasher.update(&[2]),
        };
        hasher.update(&[
            *overflow_checks as u8,
            *debug_assertions as u8,
            *panic as u8,
            *debuginfo as u8,
            *fold as u8,
            *counted as u8,
            *mir_inline as u8,
        ]);
        let Sanitize { sanitizers, recover, track_origins, coverage } = sanitize;
        hasher.update(&[sanitizers.0, recover.0, *track_origins, coverage.level]);
        hasher.update(&coverage.flags.to_le_bytes());
        match pgo {
            Pgo::None => hasher.update(&[0]),
            Pgo::Generate => hasher.update(&[1]),
            Pgo::Use(profile) => hasher.update(&[2]).id(profile),
        };
        Settings(hasher.finish())
    }

    /// The settings of the same function at another backend: the first tier's of a function
    /// tiered up (#807), or its last.
    pub fn at(&self, backend: Backend) -> Spec {
        // clone: a second spec of one function, kept beside the first (a tiered one's tiers)
        Spec { backend, ..self.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn spec() -> Spec {
        Spec {
            toolchain: Toolchain {
                id: ContentId([1; 32]),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                arch: Arch::X86_64,
                cpu: "x86-64".to_owned(),
                features: vec!["+sse2".to_owned()],
                cranelift: "0.136".to_owned(),
                llvm: Some("23.1".to_owned()),
                stencils: Some("ab".repeat(32)),
                frame_pointers: false,
                relocation: Relocation::Pic,
            },
            backend: Backend::Cranelift { speed: false },
            overflow_checks: true,
            debug_assertions: true,
            panic: Panic::Unwind,
            debuginfo: DebugInfo::Full,
            fold: false,
            counted: false,
            mir_inline: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
        }
    }

    #[test]
    fn every_field_is_in_the_settings() {
        let base = spec();
        let changes: Vec<fn(&mut Spec)> = vec![
            |it| it.toolchain.id = ContentId([3; 32]),
            |it| it.toolchain.target = "aarch64-unknown-linux-gnu".to_owned(),
            |it| it.toolchain.arch = Arch::Aarch64,
            |it| it.toolchain.cpu = "znver5".to_owned(),
            |it| it.toolchain.features.push("+avx2".to_owned()),
            |it| it.toolchain.cranelift = "0.137".to_owned(),
            |it| it.toolchain.llvm = None,
            |it| it.toolchain.llvm = Some("24.0".to_owned()),
            |it| it.toolchain.stencils = None,
            |it| it.toolchain.stencils = Some("cd".repeat(32)),
            |it| it.toolchain.frame_pointers = true,
            |it| it.toolchain.relocation = Relocation::Static,
            |it| it.backend = Backend::Llvm { opt: 3 },
            |it| it.backend = Backend::Cranelift { speed: true },
            |it| it.backend = Backend::Stencil,
            |it| it.overflow_checks = false,
            |it| it.debug_assertions = false,
            |it| it.panic = Panic::Abort,
            |it| it.debuginfo = DebugInfo::LineTablesOnly,
            |it| it.fold = true,
            |it| it.counted = true,
            |it| it.mir_inline = true,
            |it| it.sanitize.sanitizers = Sanitizers::ADDRESS,
            |it| it.sanitize.recover = Sanitizers::ADDRESS,
            |it| it.sanitize.track_origins = 2,
            |it| it.sanitize.coverage.level = 3,
            |it| it.sanitize.coverage.flags = Coverage::PC_TABLE,
            |it| it.pgo = Pgo::Generate,
            |it| it.pgo = Pgo::Use(ContentId([5; 32])),
        ];
        let mut seen = std::collections::BTreeSet::from([base.settings()]);
        for change in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert!(seen.insert(changed.settings()), "{changed:?} has the settings of another");
        }
    }

    #[test]
    fn features_are_framed() {
        let mut a = spec();
        let mut b = spec();
        a.toolchain.features = vec!["+a".to_owned(), "+b".to_owned()];
        b.toolchain.features = vec!["+a+b".to_owned()];
        assert_ne!(a.settings(), b.settings());
    }

    #[test]
    fn levels_of_llvm_are_settings() {
        let a = spec().at(Backend::Llvm { opt: 2 });
        let b = spec().at(Backend::Llvm { opt: 3 });
        assert_ne!(a.settings(), b.settings());
    }
}
