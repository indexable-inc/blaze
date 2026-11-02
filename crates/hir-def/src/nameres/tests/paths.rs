//! Path resolution against a reference resolver written here from the rules alone: every
//! namespace of the first segment, `ItemScope::get` per segment, an enum's declaration found by
//! scanning its module, a new path for each def map a lookup continues in. It runs over fixed
//! fixtures and seeded random crates (re-exports, globs, enums, blocks with items, preludes,
//! another crate, `macro_rules!`, the test harness), comparing the whole result.
//!
//! Then the cost of one lookup: one probe per segment, and no allocation for a resolved
//! three-segment path or for any shape of path on a warm def map.

use std::{fmt::Write as _, hint::black_box};

use base_db::Db;
use blaze_alloc_count::assert_at_most;
use blaze_ids::rng::SplitMix64;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use span::Edition;
use hir_expand::fixture;

use crate::{
    AdtId, Lookup, ModuleDefId, ModuleId,
    item_scope::{BUILTIN_SCOPE, ImportOrExternCrate},
    nameres::{
        BlockInfo, BuiltinShadowMode, DefMap, LocalDefMap, crate_def_map, crate_local_def_map,
        path_resolution::{PathRead, ReachedFixedPoint, ResolveMode, ResolvePathResult},
    },
    per_ns::PerNs,
    test_db::{TestDB, TestDBExt},
    visibility::Visibility,
};

#[derive(Debug, PartialEq, Eq)]
struct Res {
    def: PerNs,
    segment_index: Option<usize>,
    fixed: bool,
    differing_crate: bool,
    enum_variant: bool,
}

impl Res {
    fn empty(fixed: bool) -> Res {
        Res {
            def: PerNs::none(),
            segment_index: None,
            fixed,
            differing_crate: false,
            enum_variant: false,
        }
    }

    fn of(def: PerNs, segment_index: Option<usize>) -> Res {
        Res { def, segment_index, ..Res::empty(true) }
    }

    fn merge(self, new: Res) -> Res {
        Res {
            def: self.def.or(new.def),
            segment_index: match (self.segment_index, new.segment_index) {
                (Some(old), Some(new)) => Some(old.max(new)),
                (old, new) => old.or(new),
            },
            fixed: self.fixed || new.fixed,
            differing_crate: self.differing_crate || new.differing_crate,
            enum_variant: self.enum_variant || new.enum_variant,
        }
    }
}

impl From<ResolvePathResult> for Res {
    fn from(it: ResolvePathResult) -> Res {
        Res {
            def: it.resolved_def,
            segment_index: it.segment_index,
            fixed: it.reached_fixedpoint == ReachedFixedPoint::Yes,
            differing_crate: it.prefix_info.differing_crate,
            enum_variant: it.prefix_info.enum_variant,
        }
    }
}

#[derive(Clone, Copy)]
struct Reference<'a> {
    db: &'a Db,
    local: &'a LocalDefMap,
    mode: ResolveMode,
    shadow: BuiltinShadowMode,
}

impl Reference<'_> {
    fn fp(self, map: &DefMap, module: ModuleId, kind: PathKind, segments: &[Name]) -> Res {
        let db = self.db;
        let mut res = self.single(map, module, kind, segments);
        let (mut map, mut module) = (map, module);
        loop {
            match map.block {
                Some(BlockInfo { parent, .. }) if module == map.root => {
                    module = parent;
                    map = parent.def_map(db);
                    res = res.merge(self.single(map, module, kind, segments));
                }
                Some(_) => {
                    let root = crate_def_map(db, map.krate);
                    return res.merge(self.preludes(root, map.root, kind, segments));
                }
                None => return res,
            }
        }
    }

    fn is_2015_crate_relative(self, map: &DefMap, kind: PathKind) -> bool {
        matches!(kind, PathKind::Plain | PathKind::Abs)
            && map.data.edition == Edition::Edition2015
            && (kind == PathKind::Abs || self.mode == ResolveMode::Import)
    }

    fn single(self, map: &DefMap, module: ModuleId, kind: PathKind, segments: &[Name]) -> Res {
        let db = self.db;
        let root_types = |root: ModuleId| PerNs::types(root.into(), Visibility::Public, None);
        let (first, resolved) = if self.is_2015_crate_relative(map, kind) {
            match segments.first() {
                Some(name) => {
                    let root = map.crate_root(db);
                    let from_root = root.def_map(db).module(db, root).scope(db).get(name);
                    let in_block_root = map.block.is_some() && module == map.root;
                    let res = match in_block_root {
                        true => from_root,
                        false => from_root.or_else(|| self.extern_prelude(name)),
                    };
                    (res, 1)
                }
                None => (root_types(map.crate_root(db)), 0),
            }
        } else {
            match kind {
                PathKind::Crate => (root_types(map.crate_root(db)), 0),
                PathKind::DollarCrate(krate) => (root_types(crate_def_map(db, krate).root), 0),
                PathKind::Plain => {
                    let Some(name) = segments.first() else { return Res::empty(true) };
                    let shadow = match segments.len() {
                        1 => self.shadow,
                        _ => BuiltinShadowMode::Module,
                    };
                    (self.in_module(map, module, name, shadow), 1)
                }
                PathKind::Super(lvl) => {
                    let (mut target_map, mut target) = adjust(db, map, module);
                    for _ in 0..lvl {
                        match target_map.module(db, target).parent {
                            Some(parent) => (target_map, target) = adjust(db, target_map, parent),
                            None => return Res::empty(true),
                        }
                    }
                    if map.block != target_map.block {
                        let path = ModPath::from_segments(PathKind::SELF, segments.iter().copied());
                        return self.fp(target_map, target, path.kind, path.segments());
                    }
                    (root_types(target), 0)
                }
                PathKind::Abs => {
                    let Some(name) = segments.first() else { return Res::empty(true) };
                    let res = self.extern_prelude(name);
                    if res.is_none() {
                        return Res::empty(false);
                    }
                    (res, 1)
                }
            }
        };
        self.remaining(map, module, segments, resolved, first)
    }

    fn preludes(self, map: &DefMap, module: ModuleId, kind: PathKind, segments: &[Name]) -> Res {
        let first = if self.is_2015_crate_relative(map, kind) {
            let Some(name) = segments.first() else { return Res::empty(true) };
            self.extern_prelude(name)
        } else {
            match kind {
                PathKind::Plain => {
                    let Some(name) = segments.first() else { return Res::empty(true) };
                    self.extern_prelude(name)
                        .or_else(|| macro_use(map, name))
                        .or_else(|| self.std_prelude(map, name))
                }
                PathKind::Abs => {
                    let Some(name) = segments.first() else { return Res::empty(true) };
                    let res = self.extern_prelude(name);
                    if res.is_none() {
                        return Res::empty(false);
                    }
                    res
                }
                _ => return Res::empty(true),
            }
        };
        self.remaining(map, module, segments, 1, first)
    }

    fn in_module(
        self,
        map: &DefMap,
        module: ModuleId,
        name: &Name,
        shadow: BuiltinShadowMode,
    ) -> PerNs {
        let db = self.db;
        let legacy = map
            .legacy_macro_in_textual_scope(db, module, name)
            .map_or_else(PerNs::none, |m| PerNs::macros(m, Visibility::Public, None));
        let scope = map.module(db, module).scope(db).get(name).filter_macro(db, None);
        let builtin = match map.block {
            Some(_) => PerNs::none(),
            None => BUILTIN_SCOPE.get(name).copied().unwrap_or_else(PerNs::none),
        };
        let scope_or_builtin = match shadow {
            BuiltinShadowMode::Module => scope.or(builtin),
            BuiltinShadowMode::Other => match scope.take_types() {
                Some(ModuleDefId::ModuleId(_)) => builtin.or(scope),
                _ => scope.or(builtin),
            },
        };
        let in_block_root = map.block.is_some() && module == map.root;
        legacy
            .or(scope_or_builtin)
            .or_else(|| if in_block_root { PerNs::none() } else { self.extern_prelude(name) })
            .or_else(|| macro_use(map, name))
            .or_else(|| if in_block_root { PerNs::none() } else { self.std_prelude(map, name) })
    }

    fn extern_prelude(self, name: &Name) -> PerNs {
        self.local.extern_prelude().find(|(it, _)| *it == name).map_or_else(
            PerNs::none,
            |(_, (root, extern_crate))| {
                PerNs::types(
                    root.into(),
                    Visibility::Public,
                    extern_crate.map(ImportOrExternCrate::ExternCrate),
                )
            },
        )
    }

    fn std_prelude(self, map: &DefMap, name: &Name) -> PerNs {
        let Some((prelude, _)) = map.prelude else { return PerNs::none() };
        prelude.def_map(self.db).module(self.db, prelude).scope(self.db).get(name)
    }

    fn remaining(
        self,
        map: &DefMap,
        original: ModuleId,
        segments: &[Name],
        resolved: usize,
        mut per_ns: PerNs,
    ) -> Res {
        let db = self.db;
        for i in resolved..segments.len() {
            let Some(curr) = per_ns.types else { return Res::empty(false) };
            let segment = &segments[i];
            per_ns = match curr.def {
                ModuleDefId::ModuleId(m) if m.krate(db) != map.krate => {
                    let other = m.def_map(db);
                    let path =
                        ModPath::from_segments(PathKind::SELF, segments[i..].iter().copied());
                    let inner = Reference { local: LocalDefMap::EMPTY, ..self };
                    let res = inner.fp(other, m, path.kind, path.segments());
                    let def = match other.data.proc_macro {
                        true => res
                            .def
                            .filter_visibility(|vis| vis.is_visible_from_def_map(db, map, original)),
                        false => res.def,
                    };
                    return Res {
                        def,
                        segment_index: res.segment_index.map(|s| s + i),
                        fixed: true,
                        differing_crate: true,
                        enum_variant: res.enum_variant,
                    };
                }
                ModuleDefId::ModuleId(m) => m.def_map(db).module(db, m).scope(db).get(segment),
                ModuleDefId::AdtId(AdtId::EnumId(e)) => {
                    let container = e.lookup(db).container;
                    let declared = container
                        .def_map(db)
                        .module(db, container)
                        .scope(db)
                        .types()
                        .find(|(_, it)| it.def == ModuleDefId::from(e) && it.import.is_none())
                        .map(|(_, it)| it.vis);
                    let vis = declared.unwrap_or(curr.vis);
                    let variant = e.enum_variants(db).variants.iter().find(|(n, _)| *n == segment);
                    return match variant {
                        Some(_) if i + 1 < segments.len() => Res::empty(false),
                        Some((_, &(variant, shape))) => {
                            let def = match shape.has_value_ns_ctor() {
                                true => PerNs::both(variant.into(), variant.into(), vis, None),
                                false => PerNs::types(variant.into(), vis, None),
                            };
                            Res { enum_variant: true, ..Res::of(def, None) }
                        }
                        None => Res::of(PerNs::types(e.into(), curr.vis, curr.import), Some(i)),
                    };
                }
                def => return Res::of(PerNs::types(def, curr.vis, curr.import), Some(i)),
            };
            if !is_test_harness(db, original) {
                per_ns =
                    per_ns.filter_visibility(|vis| vis.is_visible_from_def_map(db, map, original));
            }
        }
        Res::of(per_ns, None)
    }
}

fn macro_use(map: &DefMap, name: &Name) -> PerNs {
    map.macro_use_prelude().get(name).map_or_else(PerNs::none, |&(it, extern_crate)| {
        PerNs::macros(it, Visibility::Public, extern_crate.map(ImportOrExternCrate::ExternCrate))
    })
}

fn is_test_harness(db: &Db, module: ModuleId) -> bool {
    let map = module.def_map(db);
    let Some(parent) = map.module(db, module).parent else { return false };
    map.module(db, parent)
        .children(db)
        .iter()
        .any(|(name, &child)| child == module && name.as_str() == "__blaze_test_harness")
}

fn adjust<'db>(
    db: &'db Db,
    mut map: &'db DefMap,
    mut module: ModuleId,
) -> (&'db DefMap, ModuleId) {
    while module == map.root
        && let Some(BlockInfo { parent, .. }) = map.block
    {
        map = parent.def_map(db);
        module = parent;
    }
    (map, module)
}

fn name(s: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(s))
}

/// Every name the generated crates declare or import, plus builtins, crates and one unbound.
const NAMES: &[&str] = &[
    "m0",
    "m1",
    "m2",
    "m3",
    "mb",
    "S0",
    "S1",
    "f0",
    "f1",
    "E0",
    "E1",
    "K0",
    "A",
    "B",
    "C",
    "r0",
    "r1",
    "r2",
    "mac0",
    "dep",
    "pre",
    "prelude",
    "Pre",
    "Some2",
    "None2",
    "pm",
    "a",
    "X",
    "E",
    "V",
    "W",
    "P",
    "Q",
    "g",
    "u8",
    "i32",
    "__blaze_test_harness",
    "zz",
];
const VIS: &[&str] = &["", "pub ", "pub(crate) ", "pub(super) "];
const USE_PREFIXES: &[&str] =
    &["", "crate::", "self::", "super::", "super::super::", "dep::", "::dep::", "pre::prelude::"];

const DEP: &str = r#"
//- /dep.rs crate:dep
pub mod a {
    pub struct X;
    pub enum E { V, W(u8) }
    mod private { pub struct P; }
    pub use self::private::P as Q;
    pub(crate) fn g() {}
}
pub use a::E::V;
pub fn g() {}
pub struct S0;
//- /pre.rs crate:pre
pub mod prelude {
    pub struct Pre;
    pub enum Option2 { Some2(u8), None2 }
    pub use self::Option2::*;
    pub mod pm { pub struct X; }
}
"#;

fn gen_use(rng: &mut SplitMix64, out: &mut String, renames: &mut usize) {
    let vis = rng.pick(VIS).unwrap();
    let mut path = rng.pick(USE_PREFIXES).unwrap().to_string();
    for _ in 0..rng.index(3) {
        let _ = write!(path, "{}::", rng.pick(NAMES).unwrap());
    }
    match rng.index(3) {
        0 if !path.is_empty() => {
            let _ = writeln!(out, "{vis}use {path}*;");
        }
        0 | 1 => {
            let _ = writeln!(out, "{vis}use {path}{};", rng.pick(NAMES).unwrap());
        }
        _ => {
            let _ = writeln!(out, "{vis}use {path}{} as r{renames};", rng.pick(NAMES).unwrap());
            *renames += 1;
        }
    }
}

fn gen_items(rng: &mut SplitMix64, out: &mut String) {
    if rng.below(100) < 20 {
        out.push_str("macro_rules! mac0 { () => {} }\n");
    }
    let decls: [(&str, &str); 7] = [
        ("S0", "struct S0;"),
        ("S1", "struct S1 { x: u8 }"),
        ("f0", "fn f0() {}"),
        ("f1", "fn f1() {}"),
        ("E0", "enum E0 { A, B(u8), C {} }"),
        ("E1", "enum E1 { A, C }"),
        ("K0", "const K0: u8 = 0;"),
    ];
    for (_, decl) in decls {
        if rng.below(100) < 40 {
            let _ = writeln!(out, "{}{decl}", rng.pick(VIS).unwrap());
        }
    }
    let mut renames = 0;
    for _ in 0..rng.index(5) {
        gen_use(rng, out, &mut renames);
    }
}

fn gen_module(rng: &mut SplitMix64, depth: usize, out: &mut String, block: &mut bool) {
    gen_items(rng, out);
    if depth < 3 {
        for m in ["m0", "m1", "m2", "m3"] {
            if rng.below(100) < 30 {
                let _ = writeln!(out, "{}mod {m} {{", rng.pick(VIS).unwrap());
                gen_module(rng, depth + 1, out, block);
                out.push_str("}\n");
            }
        }
    }
    if !*block && (depth == 0 || rng.below(100) < 25) {
        *block = true;
        out.push_str("fn fb() {\n");
        gen_items(rng, out);
        if rng.below(100) < 70 {
            let _ = writeln!(out, "{}mod mb {{", rng.pick(VIS).unwrap());
            gen_items(rng, out);
            out.push_str("}\n");
        }
        out.push_str("struct S0;\n$0\n}\n");
    }
}

fn gen_fixture(seed: u64) -> String {
    let mut rng = SplitMix64::new(seed);
    let edition = match seed % 4 {
        0 => " edition:2015",
        _ => "",
    };
    let mut out = format!(
        "//- /main.rs crate:main{edition} deps:dep,pre\n#[prelude_import]\nuse ::pre::prelude::*;\n"
    );
    if rng.below(100) < 30 {
        out.push_str("mod __blaze_test_harness {\nuse super::*;\n");
        if rng.below(100) < 50 {
            out.push_str("use super::m0::*;\n");
        }
        out.push_str("}\n");
    }
    let mut block = false;
    gen_module(&mut rng, 0, &mut out, &mut block);
    out.push_str(DEP);
    out
}

fn gen_path(rng: &mut SplitMix64) -> ModPath {
    let kind = *rng.pick(&[
        PathKind::Plain,
        PathKind::Plain,
        PathKind::Plain,
        PathKind::Crate,
        PathKind::SELF,
        PathKind::Super(1),
        PathKind::Super(2),
        PathKind::Abs,
    ])
    .unwrap();
    let min = match kind {
        PathKind::Plain | PathKind::Abs => 1,
        _ => 0,
    };
    let len = min + rng.index(4 - min);
    ModPath::from_segments(kind, (0..len).map(|_| name(rng.pick(NAMES).unwrap())))
}

/// Resolves every path in `paths` from every module of the crate and of the block at `$0`,
/// in every mode and shadow mode, with `resolve_path_fp_with_macro` and with the reference.
/// Returns how many lookups resolved something.
fn check_fixture(fixture: &str, paths: &mut impl FnMut() -> Vec<ModPath>) -> usize {
    let fx = fixture::db(fixture);
    let position = fx.position();
    let db = fx.into_db();
    let db = &db;
    let block_module = db.module_at_position(position);
    let krate = block_module.krate(db);
    let pair = crate_local_def_map(db, krate);
    let (crate_map, local) = (pair.def_map(db), pair.local(db));
    let block_map = block_module.def_map(db);
    assert!(block_map.block.is_some(), "no block with items at `$0`:\n{fixture}");

    let mut resolved = 0;
    for map in [crate_map, block_map] {
        for (module, _) in map.modules(db) {
            for path in paths() {
                for mode in [ResolveMode::Other, ResolveMode::Import] {
                    for shadow in [BuiltinShadowMode::Module, BuiltinShadowMode::Other] {
                        let actual = Res::from(map.resolve_path_fp_with_macro(
                            local, db, mode, module, &path, shadow, None,
                        ));
                        let expected = Reference { db, local, mode, shadow }.fp(
                            map,
                            module,
                            path.kind,
                            path.segments(),
                        );
                        assert_eq!(
                            actual,
                            expected,
                            "{:?} {mode:?} {shadow:?} from {:?} in the {} map of\n{fixture}",
                            path.display_verbatim(db).to_string(),
                            module.name(db).map(|it| it.as_str().to_owned()),
                            if map.block.is_some() { "block" } else { "crate" },
                        );
                        resolved += usize::from(!actual.def.is_none());
                    }
                }
            }
        }
    }
    resolved
}

#[test]
fn paths_match_reference_on_fixtures() {
    let fixtures = [
        r#"
//- /main.rs crate:main deps:dep,pre
#[prelude_import]
use ::pre::prelude::*;
mod m0 {
    pub mod m1 { pub struct S0; pub(super) fn f0() {} pub enum E0 { A, B(u8), C {} } }
    pub use self::m1::E0 as r0;
    pub(crate) use self::m1::E0::*;
    mod m2 { pub struct S1; }
    pub use self::m2::S1;
}
use m0::*;
use dep::a::{E, Q};
pub use self::m0::m1::E0::B as r1;
mod __blaze_test_harness { use super::m0::m2::S1; }
fn fb() {
    mod mb { pub use super::S0; pub use super::super::m0 as r2; }
    struct S0;
    use m0::m1::E0;
    $0
}
"#,
        r#"
//- /main.rs crate:main edition:2015 deps:dep,pre
#[prelude_import]
use ::pre::prelude::*;
extern crate dep as r0;
mod m0 { pub use ::dep::a; pub use r0::S0; use super::m1::*; }
pub mod m1 { pub use super::m0::*; pub(crate) struct S1; }
macro_rules! mac0 { () => {} }
fn fb() {
    fn f0() {}
    mod mb { macro_rules! mac0 { () => {} } pub fn mac0() {} }
    use self::mb::*;
    $0
}
"#,
        r#"
//- /main.rs crate:main deps:dep,pre
#[prelude_import]
use ::pre::prelude::*;
mod m0 { pub mod m1 { pub mod m2 { pub struct S0; } } }
fn fb() {
    mod m0 { pub struct S1; }
    fn f1() {
        mod mb { pub use super::super::m0::m1; }
        $0
    }
}
"#,
    ];
    let mut rng = SplitMix64::new(7);
    let mut total = 0;
    for fixture in fixtures {
        let fixture = format!("{fixture}{DEP}");
        total += check_fixture(&fixture, &mut || {
            let mut paths: Vec<_> = NAMES
                .iter()
                .map(|it| ModPath::from_segments(PathKind::Plain, [name(it)]))
                .collect();
            paths.extend((0..60).map(|_| gen_path(&mut rng)));
            paths
        });
    }
    assert!(total > 300, "only {total} lookups resolved");
}

#[test]
fn paths_match_reference_on_random_crates() {
    let mut total = 0;
    for seed in 0..48 {
        let fixture = gen_fixture(seed);
        let mut rng = SplitMix64::new(seed + 1000);
        total += check_fixture(&fixture, &mut || (0..30).map(|_| gen_path(&mut rng)).collect());
    }
    assert!(total > 1000, "only {total} lookups resolved");
}

const THREE_SEGMENTS: &str = r#"
mod a { pub mod b { pub struct C; } }
"#;

fn three_segment_lookup(db: &TestDB) -> (&DefMap, &LocalDefMap, ModPath) {
    let pair = crate_local_def_map(db, db.fetch_test_crate());
    let path = ModPath::from_segments(PathKind::Plain, [name("a"), name("b"), name("C")]);
    (pair.def_map(db), pair.local(db), path)
}

#[test]
fn one_probe_per_segment() {
    let db = fixture::db(THREE_SEGMENTS).into_db();
    let (map, local, path) = three_segment_lookup(&db);
    let mut reads = Vec::new();
    let res = map.resolve_path_fp_recording(
        local,
        &db,
        ResolveMode::Other,
        map.root,
        &path,
        BuiltinShadowMode::Module,
        None,
        &mut |read| reads.push(read),
    );
    assert!(matches!(res.resolved_def.take_types(), Some(ModuleDefId::AdtId(_))));
    assert_eq!(res.segment_index, None);

    let module = |wanted: &str| {
        let (id, _) = map
            .modules(&db)
            .find(|(id, _)| id.name(&db).is_some_and(|it| it.as_str() == wanted))
            .unwrap();
        id
    };
    let expected = [
        PathRead::Slot(map.root, name("a")),
        PathRead::Slot(module("a"), name("b")),
        PathRead::Slot(module("b"), name("C")),
    ];
    assert_eq!(reads, expected);
}

#[test]
fn resolved_path_allocates_nothing() {
    let db = fixture::db(THREE_SEGMENTS).into_db();
    let (map, local, path) = three_segment_lookup(&db);
    let resolve = || {
        Res::from(map.resolve_path_fp_with_macro(
            local,
            &db,
            ResolveMode::Other,
            map.root,
            &path,
            BuiltinShadowMode::Module,
            None,
        ))
    };
    let warm = resolve();
    assert!(warm.def.take_types().is_some() && warm.segment_index.is_none());
    let res = assert_at_most(0, "a resolved three-segment path", resolve);
    assert_eq!(res, warm);
}

const SHAPES: &str = r#"
//- /main.rs crate:main deps:dep,pre
#[prelude_import]
use ::pre::prelude::*;
pub mod m {
    pub struct S;
    impl S {
        pub fn new() -> S {
            S
        }
    }
    pub enum E {
        A,
        B(u8),
    }
    pub mod inner {
        pub fn f() {}
    }
}
mod g {
    pub use super::m::*;
}
use g::*;
fn body() {
    mod mb {
        pub struct T;
    }
    $0
}
//- /dep.rs crate:dep
pub mod a {
    pub struct X;
}
//- /pre.rs crate:pre
pub mod prelude {
    pub struct Pre;
}
"#;

/// Budget: one lookup of each shape of path on a warm def map (plain, `self::`, `super::`,
/// `crate::`, through a glob, from the prelude, from the extern prelude with and without `::`, an
/// enum variant, an associated item left unresolved, and from a block's def map, both inside it
/// and out to its crate): 0 allocations.
#[test]
fn every_shape_of_resolved_path_allocates_nothing() {
    let fx = fixture::db(SHAPES);
    let position = fx.position();
    let db = fx.into_db();
    let db = &db;
    let block_module = db.module_at_position(position);
    let pair = crate_local_def_map(db, block_module.krate(db));
    let (map, local) = (pair.def_map(db), pair.local(db));
    let block_map = block_module.def_map(db);
    assert!(block_map.block.is_some(), "`$0` is not in a block with items");
    let inner = map
        .modules(db)
        .find(|(id, _)| id.name(db).is_some_and(|it| it.as_str() == "inner"))
        .map(|(id, _)| id)
        .expect("the fixture has `m::inner`");
    let path = |kind: PathKind, segments: &[&str]| {
        ModPath::from_segments(kind, segments.iter().map(|it| name(it)))
    };
    let root = map.root;
    let lookups: [(&DefMap, ModuleId, ModPath, &str); 12] = [
        (map, root, path(PathKind::Plain, &["m", "S"]), "plain"),
        (map, inner, path(PathKind::SELF, &["f"]), "`self::`"),
        (map, inner, path(PathKind::Super(1), &["S"]), "`super::`"),
        (map, root, path(PathKind::Crate, &["m", "inner", "f"]), "`crate::`"),
        (map, root, path(PathKind::Plain, &["S"]), "through a glob"),
        (map, root, path(PathKind::Plain, &["Pre"]), "the prelude"),
        (map, root, path(PathKind::Plain, &["dep", "a", "X"]), "the extern prelude"),
        (map, root, path(PathKind::Abs, &["dep", "a", "X"]), "the extern prelude, `::`"),
        (map, root, path(PathKind::Plain, &["m", "E", "A"]), "an enum variant"),
        (map, root, path(PathKind::Plain, &["m", "S", "new"]), "an associated item"),
        (block_map, block_module, path(PathKind::Plain, &["mb", "T"]), "in a block"),
        (block_map, block_module, path(PathKind::Plain, &["m", "S"]), "a block's crate"),
    ];
    let resolve = |(map, from, path, _): &(&DefMap, ModuleId, ModPath, &str)| {
        Res::from(map.resolve_path_fp_with_macro(
            local,
            db,
            ResolveMode::Other,
            *from,
            path,
            BuiltinShadowMode::Module,
            None,
        ))
    };
    let warm: Vec<Res> = lookups.iter().map(resolve).collect();
    for ((_, _, _, what), res) in lookups.iter().zip(&warm) {
        assert!(res.fixed && !res.def.is_none(), "{what}: does not resolve: {res:?}");
    }
    let shape = |what: &str| &warm[lookups.iter().position(|it| it.3 == what).unwrap()];
    assert!(
        matches!(
            shape("through a glob").def.types,
            Some(it) if matches!(it.import, Some(ImportOrExternCrate::Glob(_)))
        ),
        "`S` at the root is not glob-imported: {:?}",
        shape("through a glob")
    );
    assert!(shape("the extern prelude").differing_crate);
    assert!(shape("the extern prelude, `::`").differing_crate);
    assert!(shape("an enum variant").enum_variant);
    assert_eq!(shape("an associated item").segment_index, Some(2));
    for (it, res) in lookups.iter().zip(&warm) {
        if it.3 != "an associated item" {
            assert_eq!(res.segment_index, None, "{}: not resolved to its end", it.3);
        }
    }
    assert_at_most(0, "a lookup of each shape of path", || {
        for it in &lookups {
            black_box(resolve(it));
        }
    });
}
