//! A `mod x;` of the project's own code whose file is at neither of its places (`x.rs`,
//! `x/mod.rs`), or whose `#[path]` names no file of the checkout: an ERROR finding of the
//! crate's names record at the `mod` item (rustc's E0583; rules 19 and 81). The module is empty
//! for everything else, and the finding goes when the file appears (the walk's look for it is
//! the record's absent read, names/scope.rs `absent`).
//!
//! The crate's walk decides it (blaze-scope task/lower.rs `walk`): a cfg'd-out `mod` is skipped
//! before its file is looked for, so `#[cfg(test)] mod t;` without `t.rs` is no finding of the
//! library, only of its test build. Placed item-relative like a failed call
//! ([`super::failed_call`], rule 71): the file, the `mod` item's chain, the edition, then the
//! rule's arguments (blaze-report's Drive words 0xC0F7 "this module's file is at neither of its
//! places: {0} or {1}", 0xC0F8 "this module's `#[path]` names {0}"), checkout-relative. The
//! range is `mod x` in the item, as rustc marks it.
//!
//! The library and its test build are two names records; equal findings of both are one printed
//! line (engine_lines.rs, the `seen` fold).

use blaze_own::SharedAlloc;
use blaze_scope::resolve::{CrateNames, NamesNote};
use blaze_scope::task::{Loc, LowerNoteKind, WalkNote};

/// `#[cfg(version(..))]` or `#[cfg_attr(version(..), ..)]` used without `#![feature(cfg_version)]`
/// (RFC 2523, rustc E0658; blaze-report Area::Cli rule 50).
pub(crate) const CFG_VER_GATE_RULE: u16 = 50;

/// One module-walk or cfg-version finding, as its record's finding: the rule
/// ([`blaze_drive::task::MODULE_FILE_MISSING_RULE`],
/// [`blaze_drive::task::PATH_FILE_MISSING_RULE`], or [`CFG_VER_GATE_RULE`]),
/// the range from the item's start and the placed message ([`super::place_pieces`]).
pub(crate) struct ModuleMissing {
    pub(crate) rule: u16,
    pub(crate) at: (u32, u32),
    pub(crate) message: blaze_own::Vec<u8, SharedAlloc<'static>>,
}

/// The findings of crate `c`'s missing module files and ungated `#[cfg(version(..))]` predicates
/// in its own files (a dependency's are not the project's to fix, as with failed calls), held on
/// `kept` (they leave the worker with the crate's record), sorted and deduplicated by
/// `(rule, message, range)` (rule 25).
pub(crate) fn findings(c: &CrateNames<'_>, kept: SharedAlloc<'static>) -> blaze_own::Vec<ModuleMissing, SharedAlloc<'static>> {
    let mut own = blaze_own::Vec::new_in(kept);
    let mut push = |rule: u16, at: Loc, places: [&str; 2]| {
        let Some(file) = c.files.get(at.file as usize).copied() else { return };
        if !file.path.starts_with("/0/") {
            return;
        }
        let mut message = blaze_own::Vec::new_in(kept);
        let mut range = (0u32, 0u32);
        match crate::work::item_holding(&file.outline, at.offset) {
            Some(item) => {
                let from = at.offset.saturating_sub(item.range.start);
                // `mod x`: from the keyword before the name to the name's end (the name's range
                // is entry-relative); an item with no name is marked at its start.
                let entry = item.range.of(file.text);
                range = match item.name {
                    Some(name) => {
                        let head = entry.get(..name.start as usize).map_or("", str::trim_end);
                        let start = match head.strip_suffix("mod") {
                            Some(before) => before.len() as u32,
                            None => name.start,
                        };
                        (start, name.end)
                    }
                    None => (from, from),
                };
                message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
                message.push(0x1F);
                let chain = crate::work::item_chain(&file.outline, item, file.text);
                for shift in (0..16).rev() {
                    message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                }
            }
            None => message.push(0x1F),
        }
        message.push(0x1F);
        message.push(b'0' + c.edition.byte());
        let mut any_place = false;
        for place in places.iter().filter(|p| !p.is_empty()) {
            message.push(0x1F);
            message.extend_from_slice(place.trim_start_matches("/0/").as_bytes());
            any_place = true;
        }
        if !any_place {
            message.push(0x1F);
        }
        own.push(ModuleMissing { rule, at: range, message });
    };
    for note in c.tree.notes.iter() {
        let (rule, at, places): (u16, _, [&str; 2]) = match note {
            WalkNote::NeitherFile { at, rs, mod_rs, .. } => (blaze_drive::task::MODULE_FILE_MISSING_RULE, *at, [rs.as_str(), mod_rs.as_str()]),
            WalkNote::PathFileNotFound { at, path } => (blaze_drive::task::PATH_FILE_MISSING_RULE, *at, [path.as_str(), ""]),
            WalkNote::UnstableCfgVersion { at } => (CFG_VER_GATE_RULE, *at, ["", ""]),
            // Not a missing file or feature-gate error: the walk's own limits and choices, each its own matter.
            WalkNote::BothFiles { .. } | WalkNote::TooManyModules { .. } | WalkNote::PathValueRefused { .. } => continue,
        };
        push(rule, at, places);
    }
    for note in c.notes.iter() {
        if let NamesNote::Lower(low) = note
            && matches!(low.kind, LowerNoteKind::UnstableCfgVersion)
        {
            push(CFG_VER_GATE_RULE, low.at, ["", ""]);
        }
    }
    own.sort_unstable_by(|a, b| (a.rule, &a.message[..], a.at).cmp(&(b.rule, &b.message[..], b.at)));
    own.dedup_by(|a, b| a.rule == b.rule && a.at == b.at && a.message[..] == b.message[..]);
    own
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_id::{ContentId, CrateItemKey};
    use blaze_scope::graph::Sysroot;
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
    use blaze_scope::task::Serial;
    use blaze_surface::Edition;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};

    struct Texts(&'static [(&'static str, &'static str)]);
    impl<'a> SourceTexts<'a> for Texts {
        fn read(&self, _: usize, path: &str) -> Option<&'a str> {
            self.0.iter().find(|&&(p, _)| p == path).map(|&(_, s)| s)
        }
    }

    fn run_findings(files: &'static [(&'static str, &'static str)]) -> std::vec::Vec<(u16, (u32, u32), std::vec::Vec<u8>)> {
        let texts = Texts(files);
        let input = CrateInput {
            key: CrateItemKey(ContentId([1; 32])),
            name: "demo",
            edition: Edition::E2021,
            root: "/0/src/lib.rs",
            shown_from: "/0/",
            features: &[],
            cfg: &[],
            deps: &[],
            implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
            script: None,
        };
        let sb = bstd::mem::SharedBump::new(4 * 1024 * 1024, bstd::mem::Tag::Nameres).expect("bump");
        let names = resolve_crates(&[input], Sysroot::default(), &texts, &Serial, blaze_own::Bump::of(&sb)).expect("resolves");
        let kept = blaze_own::testing::heap().shared();
        findings(names.krate(0), kept)
            .iter()
            .map(|f| (f.rule, f.at, f.message.to_vec()))
            .collect()
    }

    #[test]
    fn ungated_cfg_version_on_mod_and_item_surfaces_e0658() {
        // 1. Ungated `#[cfg(version("1.76"))] mod m;` (walk note) MUST surface E0658, not silently drop `mod m`.
        let on_mod = run_findings(&[("/0/src/lib.rs", "#![no_core]\n#[cfg(version(\"1.76\"))]\nmod m;\n")]);
        assert_eq!(on_mod.len(), 1, "expected 1 E0658 finding for ungated cfg(version) on mod, got {on_mod:?}");
        assert_eq!(on_mod[0].0, CFG_VER_GATE_RULE);
        let (place, words) = crate::names::place_pieces(&on_mod[0].2).expect("valid place_pieces");
        assert_eq!(place.map(|(p, _, _)| p), Some("src/lib.rs"));
        assert!(words.is_empty());

        // 2. Ungated `#[cfg(version("1.76"))] pub struct S;` (lower note) surfaces E0658.
        let on_item = run_findings(&[("/0/src/lib.rs", "#![no_core]\n#[cfg(version(\"1.76\"))]\npub struct S;\n")]);
        assert_eq!(on_item.len(), 1, "expected 1 E0658 finding for ungated cfg(version) on item, got {on_item:?}");
        assert_eq!(on_item[0].0, CFG_VER_GATE_RULE);
        let (place, words) = crate::names::place_pieces(&on_item[0].2).expect("valid place_pieces");
        assert_eq!(place.map(|(p, _, _)| p), Some("src/lib.rs"));
        assert!(words.is_empty());

        // 3. Ungated file-level `#![cfg(version("1.76"))]` deduplicates walk+lower notes to 1 E0658 finding.
        let on_file = run_findings(&[("/0/src/lib.rs", "#![no_core]\n#![cfg(version(\"1.76\"))]\npub struct S;\n")]);
        assert_eq!(on_file.len(), 1, "expected 1 deduplicated E0658 finding for ungated crate cfg(version), got {on_file:?}");
        assert_eq!(on_file[0].0, CFG_VER_GATE_RULE);
        let (_, words) = crate::names::place_pieces(&on_file[0].2).expect("valid place_pieces");
        assert!(words.is_empty());

        // 4. With `#![feature(cfg_version)]` (even AFTER `#![cfg(version("1.76"))]`) or inside disabled outer cfg: 0 findings.
        let gated_ok = run_findings(&[
            ("/0/src/lib.rs", "#![no_core]\n#![cfg(version(\"1.76\"))]\n#![feature(cfg_version)]\n#[cfg(version(\"1.76\"))]\nmod m;\n"),
            ("/0/src/m.rs", "pub struct Ok;\n"),
        ]);
        assert!(gated_ok.is_empty(), "enabled cfg_version must emit 0 findings, got {gated_ok:?}");

        let disabled_outer = run_findings(&[(
            "/0/src/lib.rs",
            "#![no_core]\n#[cfg(rust_nightly)]\nmod nightly {\n    #[cfg(version(\"1.76\"))]\n    pub struct Inner;\n}\n",
        )]);
        assert!(disabled_outer.is_empty(), "disabled outer cfg must emit 0 findings, got {disabled_outer:?}");
    }
}
