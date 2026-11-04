//! Field order by heat (`hir_ty::layout::field_order`) on programs: a plan installed as a
//! build installs it, layouts asked of the layout query, a program run by the MIR interpreter.

use std::{alloc::Global, sync::Arc};

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::{
    AdtId, HasModule, ModuleDefId, VariantId,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TypeAliasSignature},
};
use hir_expand::{EditionedFileId, fixture, name::Name};
use rustc_type_ir::inherent::GenericArgs as _;

use crate::{
    ParamEnvAndCrate,
    db::HirDatabase,
    layout::{
        Layout,
        field_order::{self, AdtPlan, FieldOrderPlan, heat::Heat},
    },
    mir::interpret_mir,
    next_solver::{DbInterner, GenericArgs},
    setup_tracing,
    test_db::{TestDB, TestDBExt as _},
};

use super::current_machine_data_layout;

struct Fx {
    db: TestDB,
    files: Vec<EditionedFileId>,
}

fn fx(#[rust_analyzer::rust_fixture] source: &str, minicore: &str) -> Fx {
    let _tracing = setup_tracing();
    let layout = current_machine_data_layout();
    let fixture =
        format!("//- target_data_layout: {layout}\n{minicore}//- /main.rs crate:test\n{source}");
    let fx = fixture::db(&fixture);
    let files = fx.files().to_vec();
    Fx { db: fx.into_db(), files }
}

impl Fx {
    fn declarations(&self) -> Vec<ModuleDefId> {
        let db = &self.db;
        self.files
            .iter()
            .flat_map(|file| {
                let module = db.module_for_file(file.file_id(db));
                let def_map = module.def_map(db);
                def_map.module(db, module).scope(db).declarations().collect::<Vec<_>>()
            })
            .collect()
    }

    fn adt(&self, name: &str) -> AdtId {
        let db = &self.db;
        self.declarations()
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::AdtId(adt @ AdtId::StructId(s))
                    if StructSignature::of(db, s).name.as_str() == name =>
                {
                    Some(adt)
                }
                ModuleDefId::AdtId(adt @ AdtId::EnumId(e))
                    if EnumSignature::of(db, e).name.as_str() == name =>
                {
                    Some(adt)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no struct or enum `{name}`"))
    }

    fn function(&self, name: &str) -> hir_def::FunctionId {
        let db = &self.db;
        self.declarations()
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::FunctionId(f)
                    if FunctionSignature::of(db, f).name.as_str() == name =>
                {
                    Some(f)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no fn `{name}`"))
    }

    /// Installs the plan that ranks, for each named ADT, its fields `hot` (per variant) first.
    fn plan(&mut self, ranks: &[(&str, &[&[&str]])]) {
        let mut by_adt = HashMap::new_in(Global);
        for &(adt, hot) in ranks {
            let variants = hot
                .iter()
                .map(|it| it.iter().map(|&name| Name::new_root(name)).collect::<Box<[Name]>>());
            by_adt.insert(self.adt(adt), AdtPlan { variants: variants.collect() });
        }
        let id = ContentId::of("test field order", format!("{ranks:?}").as_bytes());
        field_order::install(&mut self.db, Some(Arc::new(FieldOrderPlan::new(id, by_adt))));
    }

    /// The layout of the struct, enum or type alias `name`.
    fn layout(&self, name: &str) -> Layout {
        let db = &self.db;
        blaze_query::db::attach(db, || {
            let interner = DbInterner::new_no_crate(db);
            let (ty, def, krate) = match self.declarations().into_iter().find_map(|it| match it {
                ModuleDefId::TypeAliasId(t)
                    if TypeAliasSignature::of(db, t).name.as_str() == name =>
                {
                    Some(t)
                }
                _ => None,
            }) {
                Some(alias) => (
                    db.ty(alias.into()).instantiate_identity().skip_norm_wip(),
                    hir_def::GenericDefId::TypeAliasId(alias),
                    alias.krate(db),
                ),
                None => {
                    let adt = self.adt(name);
                    let args = GenericArgs::identity_for_item(interner, adt.into());
                    let ty = crate::next_solver::Ty::new_adt(interner, adt, args);
                    (ty, hir_def::GenericDefId::AdtId(adt), adt.krate(db))
                }
            };
            let env = ParamEnvAndCrate { param_env: db.trait_environment(def), krate };
            db.layout_of_ty(ty.store(), env.store()).unwrap().0.clone()
        })
    }

    /// The offset of field `field` of struct `strukt`.
    fn offset(&self, strukt: &str, field: &str) -> u64 {
        let AdtId::StructId(s) = self.adt(strukt) else { panic!("`{strukt}` is no struct") };
        let id = VariantId::from(s).fields(&self.db).field(&Name::new_root(field)).unwrap();
        let layout = self.layout(strukt);
        layout.fields.offset(u32::from(id.into_raw()) as usize).bytes()
    }

    /// What `main` writes to stdout under the MIR interpreter.
    fn stdout(&self) -> String {
        let db = &self.db;
        let main = self.function("main");
        blaze_query::db::attach(db, || {
            let env = ParamEnvAndCrate {
                param_env: db.trait_environment(main.into()),
                krate: main.krate(db),
            }
            .store();
            let body = db
                .monomorphized_runtime_mir_body(main.into(), GenericArgs::empty().store(), env)
                .unwrap_or_else(|err| panic!("`main` did not lower: {err:?}"));
            let (result, output) =
                interpret_mir(db, body, None).unwrap_or_else(|err| panic!("{err:?}"));
            result.unwrap_or_else(|err| panic!("`main` failed: {err:?}"));
            output.stdout().into_owned()
        })
    }
}

/// 76 bytes of fields, the one hot code reads declared last: rustc sorts it last, at 72.
const HOT_LAST: &str = r#"
struct Goal {
    a: [u64; 4],
    b: [u64; 4],
    c: u64,
    hot: u32,
}
"#;

#[test]
fn without_a_plan_the_layout_is_rustcs() {
    let fresh = fx(HOT_LAST, "").layout("Goal");
    let mut none = fx(HOT_LAST, "");
    field_order::install(&mut none.db, None);
    assert_eq!(none.layout("Goal"), fresh);
    // A plan that ranks only other types leaves this one alone.
    let other = format!("{HOT_LAST}\nstruct Other {{ x: [u64; 9], y: u8 }}");
    let mut ranked_other = fx(&other, "");
    ranked_other.plan(&[("Other", &[&["y"]])]);
    assert_eq!(ranked_other.layout("Goal"), fresh);
}

#[test]
fn a_hot_field_declared_last_is_laid_out_first() {
    let rustc = fx(HOT_LAST, "");
    assert_eq!(rustc.offset("Goal", "hot"), 72, "rustc's order puts `hot` last");
    let mut planned = fx(HOT_LAST, "");
    planned.plan(&[("Goal", &[&["hot"]])]);
    assert_eq!(planned.offset("Goal", "hot"), 0);
    let (before, after) = (rustc.layout("Goal"), planned.layout("Goal"));
    assert_eq!((after.size, after.align), (before.size, before.align));
}

#[test]
fn the_same_plan_gives_the_same_layout() {
    let mut first = fx(HOT_LAST, "");
    first.plan(&[("Goal", &[&["hot", "c"]])]);
    let mut second = fx(HOT_LAST, "");
    second.plan(&[("Goal", &[&["hot", "c"]])]);
    assert_eq!(first.layout("Goal"), second.layout("Goal"));
    // Installing it again changes nothing.
    first.plan(&[("Goal", &[&["hot", "c"]])]);
    assert_eq!(first.layout("Goal"), second.layout("Goal"));
}

#[test]
fn types_a_program_can_rely_on_keep_rustcs_order() {
    for attr in ["#[repr(C)]", "#[repr(packed)]", "#[repr(align(16))]", "#[repr(C, align(8))]"] {
        let source = format!("{attr}\n{HOT_LAST}");
        let rustc = fx(&source, "");
        let mut planned = fx(&source, "");
        planned.plan(&[("Goal", &[&["hot"]])]);
        assert_eq!(planned.layout("Goal"), rustc.layout("Goal"), "{attr}");
    }
    let enums = r#"
#[repr(u8)]
enum Goal { A { a: [u64; 9], hot: u8 }, B(u32) }
"#;
    let rustc = fx(enums, "");
    let mut planned = fx(enums, "");
    planned.plan(&[("Goal", &[&["hot"], &[]])]);
    assert_eq!(planned.layout("Goal"), rustc.layout("Goal"), "`#[repr(u8)]` enum");
}

#[test]
fn types_around_a_reordered_one_keep_their_layout() {
    // A niche (`r`'s null) the reordered type keeps where `Option` reads it: rustc puts the
    // reference first, so `hot` can move up behind it.
    let source = r#"
struct Goal {
    a: [u64; 8],
    r: &'static u8,
    hot: u32,
}
type Opt = Option<Goal>;
struct Wrap(Opt, u8, Goal);
"#;
    let rustc = fx(source, "//- minicore: option\n");
    let mut planned = fx(source, "//- minicore: option\n");
    planned.plan(&[("Goal", &[&["hot"]])]);
    assert_eq!((rustc.offset("Goal", "r"), rustc.offset("Goal", "hot")), (0, 72));
    assert_eq!((planned.offset("Goal", "r"), planned.offset("Goal", "hot")), (0, 8));
    assert_eq!(planned.layout("Opt"), rustc.layout("Opt"));
    let (before, after) = (rustc.layout("Wrap"), planned.layout("Wrap"));
    assert_eq!(
        (after.size, after.align, after.largest_niche),
        (before.size, before.align, before.largest_niche)
    );
    assert_eq!(after.fields, before.fields, "`Wrap`'s own fields stay put");
}

#[test]
fn heat_ranks_the_field_a_hot_loop_reads() {
    let source = format!(
        "{HOT_LAST}
fn sum(g: &Goal, n: u32) -> u64 {{
    let mut s = 0;
    let mut i = 0;
    while i < n {{
        s += g.hot as u64;
        i += 1;
    }}
    s
}}
fn once(g: &Goal) -> u64 {{
    g.a[0] + g.c
}}
"
    );
    // `g.a[0]` types through the `Index` lang item, as in rustc: without it `once` is
    // `CannotIndexInto` and its MIR does not build.
    let f = fx(&source, "//- minicore: slice, index\n");
    let db = &f.db;
    let ranked = blaze_query::db::attach(db, || {
        let mut heat = Heat::new();
        heat.add_body(db, f.function("sum").into(), 1_000_000);
        heat.add_body(db, f.function("once").into(), 1_000);
        assert_eq!((heat.unlowered, heat.too_big), (0, 0));
        heat.ranked(db)
    });
    let goal = f.adt("Goal");
    let names: Vec<_> = ranked
        .iter()
        .map(|(adt, plan)| {
            let variants: Vec<Vec<&str>> =
                plan.variants.iter().map(|it| it.iter().map(|n| n.as_str()).collect()).collect();
            (*adt == goal, variants)
        })
        .collect();
    assert_eq!(names, [(true, vec![vec!["hot"]])]);
}

/// Reads and writes every field, through references, a move and a match, and prints them.
const EVERY_FIELD: &str = r#"
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}

struct Goal {
    a: [u64; 4],
    b: u16,
    c: u64,
    d: [u32; 6],
    e: u8,
    hot: u32,
}

enum Shape {
    Empty,
    Full { x: [u64; 8], small: u8, hot: u16 },
    Other(u32),
}

fn put(mut x: u64) {
    let mut buf = [b' '; 21];
    let mut i = 20;
    if x == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while x > 0 {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    unsafe { write(1, &buf[i], 21 - i) };
}

fn bump(g: &mut Goal, n: u32) {
    let mut i = 0;
    while i < n {
        g.hot += 1;
        i += 1;
    }
}

fn total(g: &Goal) -> u64 {
    g.a[0] + g.a[3] + g.b as u64 + g.c + g.d[5] as u64 + g.e as u64 + g.hot as u64
}

fn main() {
    let mut g = Goal { a: [1, 2, 3, 4], b: 5, c: 6, d: [7, 8, 9, 10, 11, 12], e: 13, hot: 14 };
    bump(&mut g, 3);
    let h = g;
    put(h.a[0]); put(h.a[1]); put(h.a[2]); put(h.a[3]);
    put(h.b as u64); put(h.c);
    put(h.d[0] as u64); put(h.d[1] as u64); put(h.d[2] as u64);
    put(h.d[3] as u64); put(h.d[4] as u64); put(h.d[5] as u64);
    put(h.e as u64); put(h.hot as u64); put(total(&h));
    let shapes = [Shape::Empty, Shape::Full { x: [1, 2, 3, 4, 5, 6, 7, 8], small: 9, hot: 10 }, Shape::Other(11)];
    let mut i = 0;
    while i < 3 {
        match &shapes[i] {
            Shape::Empty => put(0),
            Shape::Full { x, small, hot } => {
                put(x[0]); put(x[7]); put(*small as u64); put(*hot as u64);
            }
            Shape::Other(n) => put(*n as u64),
        }
        i += 1;
    }
}
"#;

#[test]
fn a_program_reading_every_field_behaves_the_same() {
    let minicore = "//- minicore: slice, index, coerce_unsized\n";
    let rustc = fx(EVERY_FIELD, minicore);
    let mut planned = fx(EVERY_FIELD, minicore);
    planned.plan(&[("Goal", &[&["hot", "e"]]), ("Shape", &[&[], &["hot", "small"], &[]])]);
    assert_ne!(planned.layout("Goal"), rustc.layout("Goal"), "the plan reorders `Goal`");
    let expected = "1 2 3 4 5 6 7 8 9 10 11 12 13 17 58 0 1 8 9 10 11 ";
    assert_eq!(rustc.stdout(), expected);
    assert_eq!(planned.stdout(), expected);
}
