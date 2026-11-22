//! The two sides a toolchain is cut for and read on (decision D89; port27 WAVE-1600 "one sysroot
//! per side"): the TARGET side, the library the program is built against, and the MACRO side,
//! the library proc macros and build scripts are built against, as `wasm32-wasip1` modules run
//! in Wasmtime (rule 10).
//!
//! Each side is a type ([`TargetSide`], [`MacroSide`]), carried by the triple asked for
//! ([`SideTriple`]) and by the sysroot found for it ([`SideSysroot`]): a sysroot of one side is
//! a different type from the other's, so code that is handed the macro side's `core` cannot pass
//! it where the target side's is taken. The front end is given ONE side: two `core`s reaching it
//! are refused there as "the lang item `sized` is declared twice".
//!
//! A cut is one manifest record (the [`crate::record`] form) holding a unit list per triple; its
//! id is the BLAKE3 of its bytes, so the triples it was cut for are part of its key. The
//! triples one cut resolves come from [`cut_triples`]: each side's triple, each once, sorted;
//! a triple the pinned nightly's recorded answers do not hold (`rustc.json`'s `targets`) is
//! refused by name before anything is resolved.

use core::fmt;
use core::marker::PhantomData;

use blaze_own::{AVec, Alloc, AllocFailure};
use blaze_unit_graph::Sysroot;

use crate::record::{Role, ToolchainView, UnitKind};

/// The triple the macro side is cut and read for: proc macros and build scripts run as
/// `wasm32-wasip1` modules (blaze-proc-macro `module.rs`, `script.rs`).
pub const MACRO_TRIPLE: &str = "wasm32-wasip1";

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::TargetSide {}
    impl Sealed for super::MacroSide {}
}

/// Which side, as a value: what an error names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SideName {
    Target,
    Macro,
}

impl SideName {
    pub fn as_str(self) -> &'static str {
        match self {
            SideName::Target => "target side",
            SideName::Macro => "macro side",
        }
    }
}

/// A side as a type. Sealed: there are exactly two.
pub trait Side: sealed::Sealed + Copy + fmt::Debug {
    const NAME: SideName;
}

/// The library the program is built against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetSide;

/// The library proc macros and build scripts are built against (`wasm32-wasip1`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroSide;

impl Side for TargetSide {
    const NAME: SideName = SideName::Target;
}

impl Side for MacroSide {
    const NAME: SideName = SideName::Macro;
}

/// A triple asked for on side `S`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SideTriple<'t, S: Side> {
    triple: &'t str,
    side: PhantomData<S>,
}

impl<'t> SideTriple<'t, TargetSide> {
    /// The target side for `triple` (any triple, `wasm32-wasip1` too: a program may be built for
    /// it; its sysroot is still the target side's and never handed to a macro).
    pub fn target(triple: &'t str) -> SideTriple<'t, TargetSide> {
        SideTriple { triple, side: PhantomData }
    }
}

impl SideTriple<'static, MacroSide> {
    /// The macro side: always [`MACRO_TRIPLE`].
    pub fn macros() -> SideTriple<'static, MacroSide> {
        SideTriple { triple: MACRO_TRIPLE, side: PhantomData }
    }
}

impl<'t, S: Side> SideTriple<'t, S> {
    pub fn triple(&self) -> &'t str {
        self.triple
    }

    pub fn side(&self) -> SideName {
        S::NAME
    }
}

/// Why a cut was refused before it resolved anything.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CutRefused<'t> {
    /// Nothing was asked for: no target triple and no macro side.
    NothingAsked,
    /// `triple`, asked for on `side`, is not among the pinned nightly's recorded targets.
    NotRecorded { side: SideName, triple: &'t str },
    Alloc(AllocFailure),
}

impl fmt::Display for CutRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CutRefused::NothingAsked => f.write_str("toolchain cut: no target triple and no macro side asked for"),
            CutRefused::NotRecorded { side, triple } => write!(
                f,
                "toolchain cut: `{triple}` ({}) is not a target the pinned nightly's recorded answers hold \
                 (pins/answers/rustc.json `targets`); the pinned source is not cut for a target it was not asked about",
                side.as_str()
            ),
            CutRefused::Alloc(failure) => write!(f, "toolchain cut: {failure}"),
        }
    }
}

/// The triples one cut resolves: each target triple, then the macro side's when `macros` is
/// given, each once, sorted (so the record, and its id, do not depend on the order asked). Every
/// one must be in `recorded` (the pinned nightly's recorded targets), else the first that is not,
/// in the order asked, is refused by name with its side.
pub fn cut_triples<'t, 'a>(
    targets: &[SideTriple<'t, TargetSide>],
    macros: Option<SideTriple<'static, MacroSide>>,
    recorded: &[&str],
    alloc: Alloc<'a>,
) -> Result<AVec<'a, &'t str>, CutRefused<'t>> {
    if targets.is_empty() && macros.is_none() {
        return Err(CutRefused::NothingAsked);
    }
    let mut out: AVec<'a, &'t str> = AVec::new_in(alloc);
    out.try_reserve(targets.len() + 1).map_err(CutRefused::Alloc)?;
    let asked = targets
        .iter()
        .map(|t| -> (SideName, &'t str) { (t.side(), t.triple()) })
        .chain(macros.iter().map(|m| -> (SideName, &'t str) { (m.side(), m.triple()) }));
    for (side, triple) in asked {
        if !recorded.contains(&triple) {
            return Err(CutRefused::NotRecorded { side, triple });
        }
        if let Err(at) = out.binary_search(&triple) {
            out.insert(at, triple);
        }
    }
    Ok(out)
}

/// Why a side's sysroot is not in a manifest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SideError<'t> {
    /// The manifest was not cut for the side's triple.
    NoTarget { side: SideName, triple: &'t str },
    /// The manifest's units for the triple lack a library unit of this role.
    MissingRole { side: SideName, triple: &'t str, role: Role },
}

impl fmt::Display for SideError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SideError::NoTarget { side, triple } => write!(
                f,
                "the toolchain was not cut for `{triple}` ({}): cut it with that target (toolchain_cut's `targets`, or `macro_side` for {MACRO_TRIPLE})",
                side.as_str()
            ),
            SideError::MissingRole { side, triple, role } => {
                write!(f, "the toolchain's units for `{triple}` ({}) have no library unit of role {role:?}", side.as_str())
            }
        }
    }
}

/// One side's sysroot in one manifest: the units of `core`, `alloc`, `std`, `proc_macro` and
/// bstd for that side's triple. Of type `SideSysroot<TargetSide>` or `SideSysroot<MacroSide>`,
/// never one passed for the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SideSysroot<S: Side> {
    units: Sysroot,
    side: PhantomData<S>,
}

impl<S: Side> SideSysroot<S> {
    /// The units, for the unit graph's implicit dependencies of a crate of THIS side.
    pub fn units(&self) -> Sysroot {
        self.units
    }

    pub fn side(&self) -> SideName {
        S::NAME
    }
}

/// The sysroot of `asked`'s side in `view`: its triple's library units built for that triple
/// (`for_host` false: on the macro side the "target" of the units is `wasm32-wasip1` itself),
/// found by their crates' roles. Refused by name when the manifest has no units for the triple
/// or lacks one of the five roles.
pub fn side_sysroot<'t, S: Side>(view: &ToolchainView<'_>, asked: SideTriple<'t, S>) -> Result<SideSysroot<S>, SideError<'t>> {
    let triple = asked.triple();
    let Some(units) = view.units(triple) else {
        return Err(SideError::NoTarget { side: S::NAME, triple });
    };
    let mut seen = [false; 5];
    for unit in units {
        if unit.kind != UnitKind::Lib || unit.for_host {
            continue;
        }
        let slot = match view.krate(unit.krate).role {
            Role::Core => 0,
            Role::Alloc => 1,
            Role::Std => 2,
            Role::ProcMacro => 3,
            Role::Bstd => 4,
            Role::Library | Role::Blaze | Role::Registry => continue,
        };
        seen[slot] = true;
    }
    const ROLES: [Role; 5] = [Role::Core, Role::Alloc, Role::Std, Role::ProcMacro, Role::Bstd];
    if let Some(at) = seen.iter().position(|found| !found) {
        return Err(SideError::MissingRole { side: S::NAME, triple, role: ROLES[at] });
    }
    // Every role was seen above, so the shared finder finds all five.
    let units = crate::sysroot(view, triple, false).expect("each of the five roles was found just above");
    Ok(SideSysroot { units, side: PhantomData })
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;

    use super::{CutRefused, MACRO_TRIPLE, MacroSide, SideError, SideName, SideTriple, TargetSide, cut_triples, side_sysroot};
    use crate::record::{self, CrateIn, Role, TargetIn, UnitIn, UnitKind};
    use crate::{ToolchainId, ToolchainView};

    /// The targets the pinned nightly's answers are recorded for: the keys of
    /// pins/answers/rustc.json's `targets`, as read on 2026-11-16.
    const RECORDED: [&str; 12] = [
        "aarch64-apple-darwin",
        "aarch64-pc-windows-msvc",
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "wasm32-unknown-unknown",
        "wasm32-wasip1",
        "wasm32-wasip2",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-gnu",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
    ];

    /// A one-crate manifest (`core` only) cut for `triple` with the one cfg atom `target_os`.
    fn manifest(triple: &str, os: &str, alloc: Alloc<'_>) -> std::vec::Vec<u8> {
        let crates = [CrateIn {
            name: "core",
            version: "0.0.0",
            edition: "2024",
            lib: "core",
            root: "src/lib.rs",
            dir: "core",
            role: Role::Core,
            pack_kind: 3,
            pack_len: 0,
            pack: [3; 32],
        }];
        let units = [UnitIn { krate: 0, kind: UnitKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps: &[], sysroot: crate::ImplicitSysroot::None }];
        let cfg = [("target_os", Some(os))];
        let targets = [TargetIn { triple, units: &units, cfg: &cfg }];
        record::write(&crates, &targets, alloc).expect("a record").to_vec()
    }

    /// The Rust Reference, Conditional compilation, `target_os`: "Key-value option set once with
    /// the target's operating system." The library cut for one target is compiled under that
    /// target's options, so a cut for `wasm32-wasip1` is not the cut for `aarch64-apple-darwin`:
    /// its key (the manifest's id) must differ, and the same cut asked twice has one key.
    /// Expected line: `test side::tests::the_cut_key_differs_by_target ... ok`.
    #[test]
    fn the_cut_key_differs_by_target() {
        let alloc = blaze_own::testing::heap().alloc();
        let darwin = manifest("aarch64-apple-darwin", "macos", alloc);
        let wasi = manifest(MACRO_TRIPLE, "wasi", alloc);
        let darwin_id: ToolchainId = ToolchainView::id_of_bytes(&darwin);
        let wasi_id = ToolchainView::id_of_bytes(&wasi);
        assert_ne!(darwin_id, wasi_id, "two targets' cuts under one key");
        // Control: one input, one key (rule 25).
        assert_eq!(ToolchainView::id_of_bytes(&manifest("aarch64-apple-darwin", "macos", alloc)), darwin_id);
        // The triples asked are sorted, each once, so the order asked does not move the key.
        let one = cut_triples(&[SideTriple::target("aarch64-apple-darwin")], Some(SideTriple::macros()), &RECORDED, alloc).expect("both recorded");
        let two = cut_triples(&[SideTriple::target(MACRO_TRIPLE), SideTriple::target("aarch64-apple-darwin")], Some(SideTriple::macros()), &RECORDED, alloc)
            .expect("both recorded");
        assert_eq!(one.as_slice(), ["aarch64-apple-darwin", MACRO_TRIPLE]);
        assert_eq!(two.as_slice(), one.as_slice());
    }

    /// The Rust Reference, Procedural macros: "Procedural macros run during compilation, and
    /// thus have the same resources that the compiler has." blaze runs them as `wasm32-wasip1`
    /// modules, so the macro side is that triple's library; a triple the pinned nightly's
    /// answers do not hold has no cfg set to cut its library under, and is refused by name with
    /// its side, never cut under some other target's options.
    /// Expected line: `test side::tests::a_target_the_pinned_answers_lack_is_refused_by_name ... ok`.
    #[test]
    fn a_target_the_pinned_answers_lack_is_refused_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let refused = cut_triples(&[SideTriple::target("riscv64gc-unknown-linux-gnu")], None, &RECORDED, alloc);
        assert_eq!(refused.err(), Some(CutRefused::NotRecorded { side: SideName::Target, triple: "riscv64gc-unknown-linux-gnu" }));
        let without_wasi: std::vec::Vec<&str> = RECORDED.iter().copied().filter(|t| *t != MACRO_TRIPLE).collect();
        let refused = cut_triples(&[SideTriple::target("aarch64-apple-darwin")], Some(SideTriple::macros()), &without_wasi, alloc);
        assert_eq!(refused.err(), Some(CutRefused::NotRecorded { side: SideName::Macro, triple: MACRO_TRIPLE }));
        assert_eq!(cut_triples(&[], None, &RECORDED, alloc).err(), Some(CutRefused::NothingAsked));
        // Control: a recorded target is taken.
        assert!(cut_triples(&[SideTriple::target("aarch64-apple-darwin")], None, &RECORDED, alloc).is_ok());
    }

    /// The macro side is read only from its own triple's units: a manifest cut for the target
    /// alone has no macro side (refused naming `wasm32-wasip1` and the side), and the target's
    /// units are checked for each of the five roles, the first missing one named.
    /// Expected line: `test side::tests::a_side_is_read_from_its_own_triple_only ... ok`.
    #[test]
    fn a_side_is_read_from_its_own_triple_only() {
        let alloc = blaze_own::testing::heap().alloc();
        let bytes = manifest("aarch64-apple-darwin", "macos", alloc);
        let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
        assert_eq!(side_sysroot::<MacroSide>(&view, SideTriple::macros()), Err(SideError::NoTarget { side: SideName::Macro, triple: MACRO_TRIPLE }));
        assert_eq!(
            side_sysroot::<TargetSide>(&view, SideTriple::target("aarch64-apple-darwin")),
            Err(SideError::MissingRole { side: SideName::Target, triple: "aarch64-apple-darwin", role: Role::Alloc })
        );
    }
}
