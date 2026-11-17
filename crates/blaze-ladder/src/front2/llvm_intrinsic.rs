//! `extern "llvm-intrinsic"`: a block whose declarations are LLVM's target intrinsics, each
//! called by its `link_name`.
//!
//! The pinned library writes every target intrinsic of `core::arch` this way; the first one on
//! every program's path is `core::hint::spin_loop`'s `__isb` (library/stdarch/crates/core_arch/
//! src/arm_shared/barrier/mod.rs):
//!
//! ```text
//! unsafe extern "llvm-intrinsic" {
//!     #[cfg_attr(
//!         any(target_arch = "aarch64", target_arch = "arm64ec"),
//!         link_name = "llvm.aarch64.isb"
//!     )]
//!     #[cfg_attr(target_arch = "arm", link_name = "llvm.arm.isb")]
//!     fn isb(_: i32);
//! }
//! ```
//!
//! and the NEON ones declare theirs in the body of the fn that calls them (library/stdarch/
//! crates/core_arch/src/aarch64/neon/generated.rs, `vqtbl1q`): `unsafe extern "llvm-intrinsic"
//! { #[cfg_attr(any(target_arch = "aarch64", target_arch = "arm64ec"), link_name =
//! "llvm.aarch64.neon.tbl1.v16i8")] fn _vqtbl1q(a: int8x16_t, b: uint8x16_t) -> int8x16_t; }`.
//!
//! The rules (no Reference chapter covers this ABI; the unstable book and the ABI's own table
//! do): the pinned toolchain's unstable book, share/doc/rust/html/unstable-book/
//! language-features/link-llvm-intrinsics.html: "`link_llvm_intrinsics`: Allows using
//! `#[link_name="llvm.*"]`." The ABI table rustc names it in (crates/rustc_abi/src/
//! extern_abi.rs): `LlvmIntrinsic` "Unstable ABI used to call LLVM intrinsics.", spelled
//! `"llvm-intrinsic"`. So a declaration of this ABI IS the LLVM intrinsic its `link_name`
//! names: a name not starting `llvm.` (or no `link_name` at all, which would make the fn's own
//! name the symbol) names no intrinsic, and is refused by name here rather than linked as a C
//! symbol.
//!
//! What is here: the ABI's spelling and the ONE reading of such a declaration's symbol
//! ([`symbol`]), used where a fn's row is tabled (program/tabled.rs `row`), for module-level
//! and body-level declarations alike. A symbol in blaze-build's set (intrinsics/target.rs
//! `named`) is tabled as `Intrinsic::Target` and built in place by the walk; any other is
//! refused as "a target intrinsic" ([`super::program::Front2Refusal::TargetIntrinsic`]).

/// The ABI string of the block, as written between its quotes.
pub const ABI: &str = "llvm-intrinsic";

/// What every LLVM intrinsic's name starts with (the unstable book's `llvm.*`).
pub const PREFIX: &str = "llvm.";

/// Why a declaration of an `extern "llvm-intrinsic"` block names no intrinsic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// It has no `link_name` (after its `cfg_attr`s), or one that does not start `llvm.` and
    /// name something after it.
    LlvmIntrinsicName,
}

impl Refused {
    /// The refusal's words, for a front2 refusal's `why`.
    pub fn form(self) -> &'static str {
        match self {
            Refused::LlvmIntrinsicName => {
                "a declaration of an `extern \"llvm-intrinsic\"` block without a `link_name` naming an LLVM intrinsic (`llvm.` and a name)"
            }
        }
    }
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.form())
    }
}

/// The intrinsic a declaration of an `extern "llvm-intrinsic"` block names: its `link_name`
/// (as its signature's lowering read it, `cfg_attr`s applied), which must be `llvm.` followed
/// by a name.
pub fn symbol(link_name: Option<&str>) -> Result<&str, Refused> {
    match link_name {
        Some(name) if name.len() > PREFIX.len() && name.starts_with(PREFIX) => Ok(name),
        _ => Err(Refused::LlvmIntrinsicName),
    }
}

#[cfg(test)]
mod tests {
    use blaze_lower::{Cfg, CfgAnswer};
    use blaze_own::Alloc;
    use blaze_surface::Edition;

    use super::super::foreign_locals::{ForeignKind, Refused as LocalRefused, declared};
    use super::super::heads::CrateCfg;
    use super::{Refused, symbol};

    /// The atoms of aarch64-apple-darwin these tests ask: only `target_arch`. (The crate's
    /// real set comes from the target's facts; these tests ask nothing else.)
    const AARCH64: &[(&str, Option<&str>)] = &[("target_arch", Some("aarch64"))];

    /// A cfg set that answers no atom yet.
    struct Unknown;

    impl Cfg for Unknown {
        fn atom(&mut self, _name: &str, _value: Option<&str>) -> CfgAnswer {
            CfgAnswer::Need
        }
    }

    /// The unstable book, link-llvm-intrinsics.html: "Allows using `#[link_name="llvm.*"]`";
    /// rustc_abi's `LlvmIntrinsic`: "Unstable ABI used to call LLVM intrinsics." A declaration
    /// of the ABI names the intrinsic its `link_name` names: `llvm.aarch64.isb` is one; no
    /// `link_name`, a C symbol's name, or the bare prefix names none. A reading that takes
    /// the fn's own name (`isb`) as the symbol, or any `link_name`, fails the second to fourth
    /// asserts.
    /// Prints: `test front2::llvm_intrinsic::tests::llvm_intrinsic_name ... ok`
    #[test]
    fn llvm_intrinsic_name() {
        assert_eq!(symbol(Some("llvm.aarch64.isb")), Ok("llvm.aarch64.isb"));
        assert_eq!(symbol(None), Err(Refused::LlvmIntrinsicName));
        assert_eq!(symbol(Some("isb")), Err(Refused::LlvmIntrinsicName));
        assert_eq!(symbol(Some("llvm.")), Err(Refused::LlvmIntrinsicName));
    }

    /// The pinned library's `vqtbl1q` (library/stdarch/crates/core_arch/src/aarch64/neon/
    /// generated.rs) declares its intrinsic in its body under a `cfg_attr`. The Reference,
    /// "Conditional compilation", [cfg.cfg_attr.allowed-positions]: "The cfg_attr attribute
    /// may be used anywhere attributes are allowed." [cfg.cfg_attr.behavior]: "When the
    /// configuration predicate is true, cfg_attr expands out to the attributes listed after
    /// the predicate." So the block declares `_vqtbl1q` (its `link_name` is the signature's
    /// lowering's to read). Before this piece the reading refused any `cfg_attr` on such a
    /// declaration ("a `cfg` or `cfg_attr` attribute on a declaration of an extern block in a
    /// block").
    /// Prints: `test front2::llvm_intrinsic::tests::body_block_cfg_attr ... ok`
    #[test]
    fn body_block_cfg_attr() {
        let block = "unsafe extern \"llvm-intrinsic\" {\n    #[cfg_attr(\n        any(target_arch = \"aarch64\", target_arch = \"arm64ec\"),\n        link_name = \"llvm.aarch64.neon.tbl1.v16i8\"\n    )]\n    fn _vqtbl1q(a: int8x16_t, b: uint8x16_t) -> int8x16_t;\n}";
        let (abi, decls) = declared(block, Edition::E2024, &mut CrateCfg(AARCH64), Alloc::global()).expect("the block reads");
        assert_eq!(abi, "\"llvm-intrinsic\"");
        assert_eq!(decls.len(), 1);
        let d = decls[0];
        assert_eq!((d.kind, d.name, d.safe), (ForeignKind::Fn, "_vqtbl1q", false));
        assert!(d.text.starts_with("#[cfg_attr("), "its text keeps its attributes: {}", d.text);
    }

    /// The Reference, "Conditional compilation", [cfg.attr.duplicates]: "The form to which
    /// the attributes are attached will not be included if any of the cfg predicates are
    /// false"; [cfg.attr.effect]: "If any predicate is false, the form is removed from the
    /// source code." The note under [cfg.cfg_attr.attribute-list]: "The cfg_attr can expand to
    /// another cfg_attr", and what it expands to is an attribute like any written one, so a
    /// `cfg_attr` expanding to a false `cfg` removes its form too. Under `target_arch =
    /// "aarch64"` (and no `target_os`), only `on_aarch64` is declared: a reading that ignores
    /// `cfg` declares all three; one that reads `cfg_attr` only for the attributes the
    /// signature's lowering sees keeps `on_bare_aarch64`; one that removes on a true predicate
    /// declares `on_arm`.
    /// Prints: `test front2::llvm_intrinsic::tests::body_block_cfg_removes ... ok`
    #[test]
    fn body_block_cfg_removes() {
        let block = "unsafe extern \"C\" {\n    #[cfg(target_arch = \"arm\")]\n    fn on_arm(x: i32) -> i32;\n    #[cfg(any(target_arch = \"aarch64\", target_arch = \"arm64ec\"))]\n    fn on_aarch64(x: i32) -> i32;\n    #[cfg_attr(target_arch = \"aarch64\", cfg(target_os = \"none\"))]\n    fn on_bare_aarch64(x: i32) -> i32;\n}";
        let (abi, decls) = declared(block, Edition::E2024, &mut CrateCfg(AARCH64), Alloc::global()).expect("the block reads");
        assert_eq!(abi, "\"C\"");
        assert_eq!(decls.len(), 1, "one declaration stays");
        assert_eq!(decls[0].name, "on_aarch64");
    }

    /// An atom the crate's cfg set cannot answer yet is refused by name, never guessed true
    /// or false (LANE-RULES rule 7: no fallback).
    /// Prints: `test front2::llvm_intrinsic::tests::body_block_cfg_unknown_atom ... ok`
    #[test]
    fn body_block_cfg_unknown_atom() {
        let block = "extern \"C\" {\n    #[cfg(target_arch = \"aarch64\")]\n    fn f();\n}";
        let got = declared(block, Edition::E2024, &mut Unknown, Alloc::global());
        assert_eq!(got.err(), Some(LocalRefused::CfgAtom));
    }
}
