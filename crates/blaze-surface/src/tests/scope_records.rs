//! The name-resolution records' promises (`ModuleDecls`, `ModuleScope`, `CrateRoot`): each
//! working form writes a record whose `from` is the same working form, a scope's bytes are a
//! function of its bindings and not of their order (with a planted-bug control), an export
//! digest moves only with its class, and a crate root refuses one name for two crates.

use std::format;
use std::vec::Vec as StdVec;

use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};

use super::{KRATE, NAMES, SPACES, file, key, name, small};
use crate::{
    AmbiguityKind, AnonImport, Binding, BindingState, BuildError, CallRole, CallShape, ChildModDecl,
    CrateRootBuilder, CrateRootHead, CrateRootView, DeclaredItem, DeclsBuilder, Def, DefKind, Dep,
    Edition, ExternCrateDecl, ExternTarget, MacroCallDecl, MacroRulesDecl, MacroShapes, ModuleDeclsView,
    ModuleInfo, Origin, Path, PathKind, Rename, ScopeBuilder, ScopeEntry, ScopeError, ScopeView,
    Site, TextualDef, TextualMark, Unresolved, UseDecl, UseForm, UseSite, Visibility, WrittenVis,
};

fn def(tape: &Tape) -> Def {
    let n = u8::try_from(tape.below(200, "item")).expect("below 200 fits a u8");
    let kind = match tape.below(6, "kind") {
        0 => DefKind::Module,
        1 => DefKind::Enum,
        2 => DefKind::Trait,
        3 | 4 => DefKind::Macro(shapes(tape)),
        _ => DefKind::Other,
    };
    Def { key: key(n), kind }
}

/// A non-empty set of call shapes.
fn shapes(tape: &Tape) -> MacroShapes {
    let byte = u8::try_from(1 + tape.below(7, "shapes")).expect("below 8 fits a u8");
    MacroShapes::from_byte(byte).expect("one to seven is a set")
}

fn vis(tape: &Tape) -> Visibility {
    match tape.below(3, "vis") {
        0 => Visibility::Public,
        1 => Visibility::Crate(KRATE),
        _ => Visibility::Restricted(key(250)),
    }
}

/// A drawn scope's bindings: at most one per (namespace, name), in a drawn order.
pub(super) fn bindings(tape: &Tape) -> StdVec<ScopeEntry<&'static str>, Alloc<'static>> {
    let mut out = StdVec::new_in(blaze_own::testing::heap().alloc());
    for ns in SPACES {
        for name in NAMES {
            if !tape.coin("bound") {
                continue;
            }
            let state = match tape.below(2, "state") {
                0 => BindingState::Determined,
                _ => BindingState::Ambiguous(AmbiguityKind::GlobVsGlob),
            };
            let origin = [Origin::Declared, Origin::Import, Origin::Glob, Origin::ExternCrate]
                [usize::try_from(tape.below(4, "origin")).expect("below 4")];
            let expansion = u16::try_from(tape.below(3, "expansion")).expect("below 3");
            let binding = Binding { def: def(tape), vis: vis(tape), origin, expansion, state };
            out.push(ScopeEntry { ns, name, binding });
        }
    }
    let turn = usize::try_from(tape.below(u64::try_from(out.len() + 1).expect("few"), "turn")).expect("few");
    let len = out.len();
    out.rotate_left(turn.min(len));
    out
}

/// A bug planted in a scope's writer, which the order oracle must catch.
#[derive(Clone, Copy, Debug)]
pub(super) enum Plant {
    None,
    /// The writer's sorts are skipped: rows land in the order given.
    Unsorted,
}

/// The `ModuleScope` record of `key(200)` with `rows` and two children's marks, given out of
/// order.
pub(super) fn write_scope(rows: &[ScopeEntry<&str>], plant: Plant) -> std::vec::Vec<u8, Alloc<'static>> {
    let mut out = ScopeBuilder::new_in(key(200), blaze_own::testing::heap().alloc());
    out.bindings.extend_from_slice(rows);
    out.marks.extend_from_slice(&[TextualMark { child: key(202), len: 1 }, TextualMark { child: key(201), len: 0 }]);
    out.write_sorted(matches!(plant, Plant::None), blaze_own::testing::heap().alloc()).expect("a drawn scope holds one binding per slot")
}

/// Two writers of one scope, given its bindings in two orders, write equal bytes; and the
/// scope answers each binding by its name.
fn scope_agrees(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let rows = bindings(tape);
    let mut reversed = StdVec::with_capacity_in(rows.len(), blaze_own::testing::heap().alloc());
    reversed.extend(rows.iter().rev().copied());
    let bytes = write_scope(&rows, plant);
    if bytes != write_scope(&reversed, plant) {
        return Err(Broke::Oracle("two writers of one scope wrote different bytes".into()));
    }
    let buffer = Aligned::copy_from(&bytes);
    let view = ScopeView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
    for row in &rows {
        if view.get(row.ns, row.name) != Some(row.binding) {
            return Err(Broke::Oracle(format!("the scope does not answer {row:?} by its name")));
        }
    }
    let marks = [TextualMark { child: key(201), len: 0 }, TextualMark { child: key(202), len: 1 }];
    if view.bindings().count() != rows.len() || !view.marks().eq(marks) {
        return Err(Broke::Oracle("the scope lists other bindings or marks than written".into()));
    }
    Ok(())
}

/// The invariant: a scope's bytes are a function of its bindings, not of their order, and
/// each binding is found by its namespace and name.
#[test]
fn a_scope_is_its_bindings_in_any_order() {
    blaze_sim::check(Config::cases(300), |tape| scope_agrees(tape, Plant::None));
}

/// The control: a writer that keeps the order it was given writes different bytes for two
/// orders, and the oracle must say so.
#[test]
fn the_oracle_catches_an_unsorted_scope() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::Unsorted, "wrote different bytes", scope_agrees);
}

/// A drawn path of every kind.
fn path(tape: &Tape) -> Path<'static, &'static str> {
    let kind = match tape.below(6, "path kind") {
        0 => PathKind::Plain,
        1 => PathKind::Crate,
        2 => PathKind::SelfMod,
        3 => PathKind::Super(u8::try_from(1 + tape.below(3, "supers")).expect("below 4")),
        4 => PathKind::Abs,
        _ => PathKind::DollarCrate(CrateItemKey(ContentId([7; 32]))),
    };
    let mut segments = Vec::new_in(blaze_own::testing::heap().alloc());
    for _ in 0..tape.below(4, "segments") {
        segments.push(name(tape));
    }
    Path { kind, segments }
}

fn site(tape: &Tape) -> UseSite {
    let expansion = tape.coin("in an expansion").then(|| key(77));
    UseSite { expansion, index: small(tape, 9, "use index") }
}

/// A drawn scope with every list filled: bindings, an error of each shape, textual
/// definitions, marks, traits and expanded `#[macro_export]`s.
fn full_scope(tape: &Tape) -> ScopeBuilder<'static, &'static str> {
    let mut out = ScopeBuilder::new_in(key(200), blaze_own::testing::heap().alloc());
    out.bindings.extend_from_slice(&bindings(tape));
    for _ in 0..tape.below(6, "errors") {
        let (ns, name_) = (SPACES[usize::try_from(tape.below(3, "ns")).expect("below 3")], name(tape));
        let error = match tape.below(9, "error") {
            0 => ScopeError::Import { site: site(tape), path: path(tape), segment: small(tape, 4, "segment"), why: Unresolved::NotFound },
            1 => ScopeError::Macro {
                call: key(90),
                path: path(tape),
                segment: 0,
                why: if tape.coin("derive after") { Unresolved::DeriveAfterAttrMacro } else { Unresolved::Ambiguous(AmbiguityKind::GlobVsOuter) },
            },
            2 => ScopeError::ExternCrate { name: name_ },
            3 => ScopeError::RestrictedNotAncestor { path: path(tape) },
            4 => ScopeError::Duplicate { ns, name: name_, first: def(tape), second: def(tape) },
            5 => ScopeError::WiderReexport { site: site(tape), ns, name: name_, def: def(tape) },
            6 => ScopeError::Ambiguous { site: Site::Call(key(91)), ns, name: name_, kind: AmbiguityKind::MacroRulesVsModularized, first: def(tape), second: def(tape) },
            7 => ScopeError::Inconsistent { site: Site::Import(site(tape)), ns, name: name_, early: def(tape), last: def(tape) },
            _ => ScopeError::RecursionLimit { call: key(92) },
        };
        out.errors.push(error);
    }
    for n in 0..u8::try_from(tape.below(4, "textual")).expect("below 4") {
        out.textual.push(TextualDef { name: name(tape), def: key(100 + n), expansion: 0, shapes: shapes(tape) });
    }
    for n in 0..u8::try_from(tape.below(3, "marks")).expect("below 3") {
        out.marks.push(TextualMark { child: key(120 + n), len: small(tape, 4, "mark") });
    }
    if tape.coin("a trait import") {
        out.traits.push(AnonImport { site: site(tape), def: def(tape), vis: vis(tape) });
    }
    if tape.coin("an expanded export") {
        out.macro_exports.push(TextualDef { name: name(tape), def: key(130), expansion: 1, shapes: shapes(tape) });
    }
    out
}

/// The round trip: a scope's record decodes to the scope (its lists sorted as the record
/// keeps them), and that writes the same bytes again.
#[test]
fn a_scope_round_trips_through_its_record() {
    blaze_sim::check(Config::cases(300), |tape| {
        let mut scope = full_scope(tape);
        let bytes = scope.write(blaze_own::testing::heap().alloc()).map_err(|error| Broke::Oracle(error.to_string()))?;
        let buffer = Aligned::copy_from(&bytes);
        let view = ScopeView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
        let back = ScopeBuilder::from(&view, blaze_own::testing::heap().alloc());
        scope.bindings.sort_unstable_by(|a, b| (a.ns, a.name).cmp(&(b.ns, b.name)));
        scope.marks.sort_unstable_by_key(|m| m.child);
        if back != scope {
            return Err(Broke::Oracle(format!("read back otherwise:\n wrote {scope:?}\n read {back:?}")));
        }
        let again = back.write(blaze_own::testing::heap().alloc()).map_err(|error| Broke::Oracle(error.to_string()))?;
        match again == bytes {
            true => Ok(()),
            false => Err(Broke::Oracle("the decoded scope writes other bytes".into())),
        }
    });
}

#[test]
fn a_globs_digest_moves_with_every_input_and_nothing_else() {
    let edge = crate::GlobEdge { source: Def { key: key(3), kind: DefKind::Module }, vis: Visibility::Public, expansion: 0 };
    let krate = CrateItemKey(ContentId([7; 32]));
    let chain = [key(1), key(0)];
    let digest = |edges: &[crate::GlobEdge], chain: &[ItemKey]| crate::globs_digest(edges, krate, chain, blaze_own::testing::heap().alloc());
    let base = digest(&[edge], &chain);
    assert_eq!(base, digest(&[edge], &chain), "a digest is a function of its inputs");
    assert_ne!(base, digest(&[crate::GlobEdge { vis: Visibility::Restricted(key(0)), ..edge }], &chain));
    assert_ne!(base, digest(&[crate::GlobEdge { expansion: 1, ..edge }], &chain));
    assert_ne!(base, digest(&[], &chain));
    assert_ne!(base, digest(&[edge], &chain[..1]));
}

/// The round trip of a module's declarations: every list, every path kind and every written
/// visibility, in source order, and the module's place.
#[test]
fn module_decls_round_trip_in_source_order() {
    blaze_sim::check(Config::cases(300), |tape| {
        let written = |tape: &Tape| match tape.below(7, "written vis") {
            0 => WrittenVis::Private,
            1 => WrittenVis::Pub,
            2 => WrittenVis::Crate,
            3 => WrittenVis::Super,
            4 => WrittenVis::SelfOnly,
            5 => WrittenVis::In(path(tape)),
            _ => WrittenVis::WithoutIn(path(tape)),
        };
        let rename = |tape: &Tape| match tape.below(3, "rename") {
            0 => Rename::Same,
            1 => Rename::To(name(tape)),
            _ => Rename::Underscore,
        };
        let mut decls = DeclsBuilder::new_in(blaze_own::testing::heap().alloc());
        for _ in 0..tape.below(4, "items") {
            let ns = SPACES[usize::try_from(tape.below(3, "ns")).expect("below 3")];
            decls.items.push(DeclaredItem { name: name(tape), ns, def: def(tape), vis: written(tape) });
        }
        for n in 0..u8::try_from(tape.below(3, "mods")).expect("below 3") {
            let macro_use = tape.coin("macro_use");
            decls.mods.push(ChildModDecl { name: name(tape), module: key(140 + n), vis: written(tape), macro_use, pos: u32::from(n) });
        }
        for n in 0..tape.below(4, "uses") {
            let leaf = match tape.coin("glob") {
                true => UseForm::Glob,
                false => UseForm::Named { rename: rename(tape), types_only: tape.coin("types only") },
            };
            let pos = 10 + u32::try_from(n).expect("below 4");
            decls.uses.push(UseDecl { path: path(tape), leaf, vis: written(tape), pos });
        }
        if tape.coin("an extern crate") {
            let target = match tape.coin("self") {
                true => ExternTarget::SelfCrate,
                false => ExternTarget::Dep(name(tape)),
            };
            decls.extern_crates.push(ExternCrateDecl { target, rename: rename(tape), vis: written(tape) });
        }
        if tape.coin("a macro_rules") {
            decls.macro_rules.push(MacroRulesDecl { name: name(tape), def: key(150), pos: 20, export: tape.coin("export"), shapes: shapes(tape) });
        }
        for n in 0..u8::try_from(tape.below(3, "calls")).expect("below 3") {
            let kind = [CallShape::FnLike, CallShape::Attr, CallShape::Derive][usize::from(n)];
            let item = (kind != CallShape::FnLike).then_some(u32::from(n));
            let role = match kind {
                CallShape::Attr => [CallRole::Call, CallRole::AfterDerive, CallRole::Keep, CallRole::AfterDeriveBeforeDerive]
                    [usize::try_from(tape.below(4, "role")).expect("below 4")],
                CallShape::FnLike | CallShape::Derive => CallRole::Call,
            };
            decls.calls.push(MacroCallDecl { call: key(160 + n), path: path(tape), kind, pos: 30 + u32::from(n), item, role });
        }
        let info = ModuleInfo { krate: KRATE, parent: Some(key(199)), name: Some(name(tape)), no_implicit_prelude: tape.coin("nip") };
        let bytes = decls.write(key(200), &info, blaze_own::testing::heap().alloc()).map_err(|error| Broke::Oracle(error.to_string()))?;
        let buffer = Aligned::copy_from(&bytes);
        let view = ModuleDeclsView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
        let back = DeclsBuilder::from(&view, blaze_own::testing::heap().alloc());
        if view.module() != key(200) || view.info() != info || back != decls {
            return Err(Broke::Oracle(format!("read back otherwise:\n wrote {decls:?}\n read {back:?}")));
        }
        match back.write(key(200), &view.info(), blaze_own::testing::heap().alloc()) {
            Ok(again) if again == bytes => Ok(()),
            Ok(_) => Err(Broke::Oracle("the decoded declarations write other bytes".into())),
            Err(error) => Err(Broke::Oracle(error.to_string())),
        }
    });
}

#[test]
fn a_crate_root_reads_back_sorted_and_refuses_two_crates_of_one_name() {
    let head = CrateRootHead {
        krate: KRATE,
        root_module: key(200),
        root_file: file(0),
        edition: Edition::E2024,
        no_std: true,
        no_core: false,
        arbitrary_ints: true,
        prelude: Some(key(201)),
    };
    let core = Dep { name: "core", krate: CrateItemKey(ContentId([7; 32])), root: key(7) };
    let alloc = Dep { name: "alloc", krate: CrateItemKey(ContentId([8; 32])), root: key(8) };
    let build = |deps: [Dep<&'static str>; 2]| {
        let mut out = CrateRootBuilder::new_in(head, blaze_own::testing::heap().alloc());
        for dep in deps {
            out.extern_prelude.push((dep.name, dep.root));
            out.deps.push(dep);
        }
        out.extern_prelude.push(("me", key(200)));
        out.tools.extend_from_slice(&["blaze", "blaze"]);
        out.features.extend_from_slice(&["never_type", "default_field_values", "never_type"]);
        out.macro_use.push(key(7));
        out.macro_exports.push(("m", key(30), MacroShapes::ATTR.with(CallShape::FnLike)));
        out
    };
    let bytes = build([core, alloc]).write(blaze_own::testing::heap().alloc()).expect("small");
    assert_eq!(bytes, build([alloc, core]).write(blaze_own::testing::heap().alloc()).expect("small"), "two orders, two records");
    let buffer = Aligned::copy_from(&bytes);
    let view = CrateRootView::read(&buffer).expect("a written record");
    assert_eq!(view.head(), head);
    assert!(view.externs().eq([("alloc", key(8)), ("core", key(7)), ("me", key(200))]));
    assert_eq!((view.extern_crate("core"), view.extern_crate("std")), (Some(key(7)), None));
    assert_eq!(view.dep("alloc"), Some(alloc));
    assert!(view.tools().eq(["blaze"]) && view.macro_use().eq([key(7)]));
    // The root's `#![feature(..)]` names, sorted and once each, found by name.
    assert!(view.features().eq(["default_field_values", "never_type"]));
    assert!(view.has_feature("default_field_values") && !view.has_feature("let_chains"));
    let back = CrateRootBuilder::from(&view, blaze_own::testing::heap().alloc());
    assert_eq!(back.write(blaze_own::testing::heap().alloc()).expect("small"), bytes, "the decoded root writes other bytes");

    let mut twice = CrateRootBuilder::new_in(head, blaze_own::testing::heap().alloc());
    twice.extern_prelude.extend_from_slice(&[("core", key(7)), ("core", key(8))]);
    assert!(matches!(twice.write(blaze_own::testing::heap().alloc()).err(), Some(BuildError::Twice { .. })));
}
