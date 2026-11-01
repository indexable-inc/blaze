//! The global-goal table and the shape table against a locked map and the plain solver
//! (docs/v2/plan/lanes/frontend/fast-trait.md, "Tests").

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    sync::Barrier,
    thread,
};

use base_db::{Crate, Db, all_crates};
use hir_def::{
    AdtId, FunctionId, HasModule, ModuleDefId, TraitId,
    nameres::crate_def_map,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::fixture;
use parking_lot::Mutex;
use rustc_type_ir::{Interner as _, MayBeErased, Upcast, inherent::Ty as _};

use super::{GoalKey, mode_tag, shape, switch, table::Table};
use crate::{
    Span,
    db::HirDatabase,
    next_solver::{
        DbInterner, GenericArgs, Mutability, ParamEnv, Predicate, SolverContext, SolverDefIds,
        TraitRef, Ty, TypingMode,
        infer::{DbInternerInferExt, select::EvaluationResult},
    },
};

/// splitmix64: a fixed sequence per seed, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A key as the table sees one: never 0 (empty), never with the pending bit.
fn key_of(bits: u64) -> GoalKey {
    GoalKey((bits >> 1) | 1)
}

fn value_of(key: GoalKey) -> u64 {
    key.bits().rotate_left(17) ^ 0xA5A5
}

#[test]
fn the_table_agrees_with_a_locked_map_under_eight_threads() {
    let table = Table::<u64>::default();
    let reference = Mutex::new(HashMap::new());
    let keys: Vec<GoalKey> = {
        let mut rng = Rng(7);
        (0..20_000).map(|_| key_of(rng.next())).collect()
    };
    let start = Barrier::new(8);
    thread::scope(|scope| {
        for worker in 0..8u64 {
            let (table, reference, keys, start) = (&table, &reference, &keys, &start);
            scope.spawn(move || {
                let mut rng = Rng(worker);
                start.wait();
                for _ in 0..6_000 {
                    let key = keys[rng.below(keys.len() as u64) as usize];
                    match table.get(key) {
                        Some(value) => assert_eq!(value, value_of(key)),
                        None => {
                            table.insert(key, value_of(key));
                            reference.lock().insert(key, value_of(key));
                        }
                    }
                }
            });
        }
    });
    let reference = reference.into_inner();
    assert!(reference.len() > 2 * 2048, "the test must fill more than the first level");
    for key in &keys {
        assert_eq!(table.get(*key), reference.get(key).copied(), "{key:?}");
    }
}

#[test]
fn an_insert_of_a_kept_key_keeps_the_first_value() {
    let table = Table::<u64>::default();
    table.insert(key_of(5), 1);
    table.insert(key_of(5), 2);
    assert_eq!(table.get(key_of(5)), Some(1));
    assert_eq!(table.get(key_of(9)), None);
}

#[test]
fn a_table_hit_allocates_nothing() {
    let table = Table::<u64>::default();
    let keys: Vec<GoalKey> = (1..1000).map(key_of).collect();
    for &key in &keys {
        table.insert(key, value_of(key));
    }
    blaze_alloc_count::assert_at_most(0, "a global-goal table hit", || {
        for &key in &keys {
            assert_eq!(table.get(key), Some(value_of(key)));
        }
    });
}

#[test]
fn an_empty_table_allocates_nothing() {
    blaze_alloc_count::assert_at_most(0, "a table no goal was kept in", || {
        let table = Table::<u64>::default();
        assert_eq!(table.get(key_of(3)), None);
    });
}

const PRELUDE: &str = r#"
//- minicore: sized, copy, clone, send, sync, unpin, phantom_data, derive, option
use core::marker::PhantomData;

pub struct NotSync(u8);
impl !Sync for NotSync {}
pub struct Pinned(u8);
impl !Unpin for Pinned {}
pub struct Wrap<T>(T);
pub struct Raw<T>(*const T);
unsafe impl<T: Send> Send for Raw<T> {}
unsafe impl<T: Sync> Sync for Raw<T> {}
pub struct Only<T>(PhantomData<fn() -> T>);
pub struct Odd<T>(T);
impl Clone for Odd<u8> {
    fn clone(&self) -> Self {
        loop {}
    }
}
pub struct List {
    next: Option<&'static List>,
    value: u8,
}
#[derive(Clone, Copy)]
pub struct Pair(u8, u8);
pub fn shadowed<'a>()
where
    Wrap<&'a u8>: Send,
{
}
"#;

/// A crate of `count` random ADTs over the prelude's: fields of every shape the walk steps
/// through, derived and written `Clone`/`Copy` impls, negative and explicit auto-trait impls.
fn generated(seed: u64, count: usize) -> String {
    let mut rng = Rng(seed);
    let mut text = PRELUDE.to_owned();
    for i in 0..count {
        let any = |rng: &mut Rng| rng.below(count as u64);
        let mut fields = Vec::new();
        for _ in 0..=rng.below(3) {
            let earlier = (i > 0).then(|| rng.below(i as u64));
            let field = match (rng.below(14), earlier) {
                (0, _) => "u8".to_owned(),
                (1, _) => "bool".to_owned(),
                (2, _) => "*const u8".to_owned(),
                (3, _) => format!("&'static S{}", any(&mut rng)),
                (4, _) => format!("PhantomData<S{}>", any(&mut rng)),
                (5, Some(j)) => format!("[S{j}; 2]"),
                (6, Some(j)) => format!("(S{j}, u8)"),
                (7, _) => "NotSync".to_owned(),
                (8, _) => "Pinned".to_owned(),
                (9, Some(j)) => format!("Wrap<S{j}>"),
                (10, _) => format!("Raw<S{}>", any(&mut rng)),
                (11, _) => format!("Only<S{}>", any(&mut rng)),
                (12, _) => format!("Option<&'static S{}>", any(&mut rng)),
                (13, _) => format!("fn(S{})", any(&mut rng)),
                _ => "List".to_owned(),
            };
            fields.push(field);
        }
        let derive = match rng.below(6) {
            0 | 1 => "#[derive(Clone)]\n",
            2 => "#[derive(Clone, Copy)]\n",
            _ => "",
        };
        if rng.below(4) == 0 {
            let variants: Vec<String> =
                fields.iter().enumerate().map(|(v, ty)| format!("V{v}({ty})")).collect();
            writeln!(text, "{derive}pub enum S{i} {{ {} }}", variants.join(", ")).unwrap();
        } else {
            writeln!(text, "{derive}pub struct S{i}({});", fields.join(", ")).unwrap();
        }
        if derive.is_empty() && rng.below(3) == 0 {
            writeln!(text, "impl Clone for S{i} {{ fn clone(&self) -> Self {{ loop {{}} }} }}")
                .unwrap();
        }
        match rng.below(10) {
            0 => writeln!(text, "impl !Send for S{i} {{}}").unwrap(),
            1 => writeln!(text, "unsafe impl Sync for S{i} {{}}").unwrap(),
            _ => {}
        }
    }
    text
}

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

fn local_crate(db: &Db) -> Crate {
    *all_crates(db).iter().find(|krate| krate.data(db).origin.is_local()).expect("a local crate")
}

/// Every module-level declaration of `crates`, by name.
fn declarations(db: &Db, crates: &[Crate]) -> BTreeMap<String, ModuleDefId> {
    let mut found = BTreeMap::new();
    for &krate in crates {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                let name = match def {
                    ModuleDefId::AdtId(AdtId::StructId(id)) => {
                        StructSignature::of(db, id).name.clone()
                    }
                    ModuleDefId::AdtId(AdtId::EnumId(id)) => EnumSignature::of(db, id).name.clone(),
                    ModuleDefId::TraitId(id) => TraitSignature::of(db, id).name.clone(),
                    ModuleDefId::FunctionId(id) => FunctionSignature::of(db, id).name.clone(),
                    _ => continue,
                };
                found.insert(name.as_str().to_owned(), def);
            }
        }
    }
    found
}

struct Names {
    adts: BTreeMap<String, AdtId>,
    traits: BTreeMap<String, TraitId>,
    functions: BTreeMap<String, FunctionId>,
}

fn names(db: &Db, krate: Crate) -> Names {
    let mut names =
        Names { adts: BTreeMap::new(), traits: BTreeMap::new(), functions: BTreeMap::new() };
    for (name, def) in declarations(db, &all_crates(db)) {
        match def {
            ModuleDefId::TraitId(id) => {
                names.traits.insert(name, id);
            }
            ModuleDefId::AdtId(id) if id.krate(db) == krate => {
                names.adts.insert(name, id);
            }
            ModuleDefId::FunctionId(id) => {
                names.functions.insert(name, id);
            }
            _ => {}
        }
    }
    names
}

const TRAITS: [&str; 6] = ["Send", "Sync", "Unpin", "Copy", "Clone", "Sized"];
const WRAPPERS: [&str; 4] = ["Wrap", "Raw", "Only", "Odd"];

/// The goals the differential asks: every trait of [`TRAITS`] of every non-generic local ADT,
/// of the built-in types and prelude wrappers around it, and of a few scalars.
fn goals<'db>(db: &'db Db, krate: Crate, interner: DbInterner<'db>) -> Vec<(TraitId, Ty<'db>)> {
    let names = names(db, krate);
    let types = interner.default_types();
    let wrapper = |name: &str, arg: Ty<'db>| {
        Ty::new_adt(interner, names.adts[name], GenericArgs::new_from_slice(&[arg.into()]))
    };
    let mut self_tys = vec![types.types.u8, types.types.str, types.types.u8_slice];
    self_tys.push(wrapper("Odd", types.types.u8));
    self_tys.push(wrapper("Odd", types.types.u16));
    for (name, &adt) in &names.adts {
        if WRAPPERS.contains(&name.as_str()) {
            continue;
        }
        let ty = Ty::new_adt(interner, adt, GenericArgs::empty());
        self_tys.extend([
            ty,
            Ty::new_tup(interner, &[ty, types.types.u8]),
            Ty::new_array(interner, ty, 2),
            Ty::new_ref(interner, types.regions.statik, ty, Mutability::Not),
            Ty::new_ptr(interner, ty, Mutability::Not),
        ]);
        self_tys.extend(WRAPPERS.map(|it| wrapper(it, ty)));
    }
    let mut goals = Vec::new();
    for ty in self_tys {
        for name in TRAITS {
            goals.push((names.traits[name], ty));
        }
    }
    goals
}

/// Each goal's evaluation, twice in a row (the second a table hit when the tables are on), in
/// the two typing modes the table keys most.
#[blaze_query::tracked(returns(deref))]
fn answers(db: &Db, krate: Crate) -> Box<[u8]> {
    let interner = DbInterner::new_with(db, krate);
    let goals = goals(db, krate, interner);
    let mut answers = Vec::new();
    for mode in [TypingMode::non_body_analysis(), TypingMode::PostAnalysis] {
        let infcx = interner.infer_ctxt().build(mode);
        for &(trait_, ty) in &goals {
            for _ in 0..2 {
                answers.push(infcx.type_implements_trait(trait_, [ty], ParamEnv::empty()) as u8);
            }
        }
    }
    answers.into()
}

/// The shape table's verdict on each goal, in the first typing mode of [`answers`].
#[blaze_query::tracked(returns(deref))]
fn shape_verdicts(db: &Db, krate: Crate) -> Box<[bool]> {
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let cx = <&SolverContext<'_>>::from(&infcx);
    let names = names(db, krate);
    let (copy, clone) = (names.traits["Copy"], names.traits["Clone"]);
    let limit = interner.recursion_limit();
    goals(db, krate, interner)
        .into_iter()
        .map(|(trait_, ty)| {
            if trait_ == copy || trait_ == clone {
                shape::copy_clone_holds(cx, trait_.into(), ty)
            } else {
                shape::auto_trait_holds(cx, trait_.into(), ty, ParamEnv::empty(), limit).is_some()
            }
        })
        .collect()
}

#[test]
fn the_tables_agree_with_the_plain_solver_on_generated_crates() {
    let mut answered = 0;
    let mut asked = 0;
    for seed in 0..8 {
        let fixture = generated(seed, 24);
        let plain_db = db_of(&fixture);
        let plain = switch::without(|| answers(&plain_db, local_crate(&plain_db)).to_vec());
        let db = db_of(&fixture);
        let krate = local_crate(&db);
        assert_eq!(answers(&db, krate), &plain[..], "seed {seed}:\n{fixture}");
        let verdicts = shape_verdicts(&db, krate);
        for (i, &holds) in verdicts.iter().enumerate() {
            if holds {
                let ok = EvaluationResult::EvaluatedToOk as u8;
                assert_eq!(plain[2 * i], ok, "seed {seed}, goal {i}: shape holds, solver does not");
            }
        }
        answered += verdicts.iter().filter(|&&it| it).count();
        asked += verdicts.len();
    }
    assert!(answered * 10 > asked, "the shape table answered {answered} of {asked}");
}

#[test]
fn a_shape_hit_allocates_nothing() {
    let db = db_of(&generated(3, 24));
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let cx = <&SolverContext<'_>>::from(&infcx);
    let names = names(&db, krate);
    let (send, copy) = (names.traits["Send"], names.traits["Copy"]);
    let pair = Ty::new_adt(interner, names.adts["Pair"], GenericArgs::empty());
    let list = Ty::new_adt(interner, names.adts["List"], GenericArgs::empty());
    let tys = [
        list,
        Ty::new_tup(interner, &[list, interner.default_types().types.u8]),
        Ty::new_array(interner, list, 2),
    ];
    let limit = interner.recursion_limit();
    for ty in tys {
        assert!(shape::auto_trait_holds(cx, send.into(), ty, ParamEnv::empty(), limit).is_some());
    }
    assert!(shape::copy_clone_holds(cx, copy.into(), pair));
    blaze_alloc_count::assert_at_most(0, "a shape-table hit", || {
        for _ in 0..100 {
            for ty in tys {
                let env = ParamEnv::empty();
                assert!(shape::auto_trait_holds(cx, send.into(), ty, env, limit).is_some());
            }
            assert!(shape::copy_clone_holds(cx, copy.into(), pair));
        }
    });
}

#[test]
fn a_region_only_auto_trait_bound_turns_the_shape_table_off() {
    let db = db_of(PRELUDE);
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let cx = <&SolverContext<'_>>::from(&infcx);
    let names = names(&db, krate);
    let send = names.traits["Send"];
    let env = db.trait_environment(names.functions["shadowed"].into());
    assert!(shape::env_may_shadow(interner, env));
    let list = Ty::new_adt(interner, names.adts["List"], GenericArgs::empty());
    let limit = interner.recursion_limit();
    assert!(shape::auto_trait_holds(cx, send.into(), list, ParamEnv::empty(), limit).is_some());
    assert_eq!(shape::auto_trait_holds(cx, send.into(), list, env, limit), None);
}

/// Links of the `Send` chain: `C{i}` holds `C{i + 1}`, the last holds `u8`, so `C{i}: Send`
/// needs `CHAIN - i` levels below it.
const CHAIN: usize = 20;
/// The link `Base: Deep` needs: 6 levels below it.
const BASE_NEEDS: usize = 14;
/// The most wrappers around `Base` the differential asks `Deep` of.
const WRAPS: usize = 16;

/// Under `#![recursion_limit = "8"]`: the chain's goals at the root, and the same chain's
/// goals nested under `W<..W<Base>..>: Deep`, each wrapper one level deeper. Both reach past
/// the limit and past twice the limit (the solver's overflow-warning rerun) within the shape
/// table's 24 levels.
fn near_the_limit() -> String {
    let mut text = "//- minicore: sized, send\n#![recursion_limit = \"8\"]\n".to_owned();
    for i in 0..CHAIN - 1 {
        writeln!(text, "pub struct C{i}(C{});", i + 1).unwrap();
    }
    writeln!(text, "pub struct C{}(u8);", CHAIN - 1).unwrap();
    text.push_str("pub struct W<T>(T);\npub struct Base;\npub trait Deep {}\n");
    text.push_str("impl<T: Deep> Deep for W<T> {}\n");
    writeln!(text, "impl Deep for Base where C{BASE_NEEDS}: Send {{}}").unwrap();
    text
}

fn limit_goals<'db>(
    db: &'db Db,
    krate: Crate,
    interner: DbInterner<'db>,
) -> Vec<(TraitId, Ty<'db>)> {
    let names = names(db, krate);
    let (send, deep) = (names.traits["Send"], names.traits["Deep"]);
    let link = |i: usize| Ty::new_adt(interner, names.adts[&format!("C{i}")], GenericArgs::empty());
    let mut goals: Vec<_> = (0..CHAIN).map(|i| (send, link(i))).collect();
    let mut ty = Ty::new_adt(interner, names.adts["Base"], GenericArgs::empty());
    for _ in 0..=WRAPS {
        goals.push((deep, ty));
        ty = Ty::new_adt(interner, names.adts["W"], GenericArgs::new_from_slice(&[ty.into()]));
    }
    goals
}

/// Each goal's evaluation and the overflow warnings it gave, twice in a row.
#[blaze_query::tracked(returns(deref))]
fn limit_answers(db: &Db, krate: Crate) -> Box<[u8]> {
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut answers = Vec::new();
    for (trait_, ty) in limit_goals(db, krate, interner) {
        for _ in 0..2 {
            answers.push(infcx.type_implements_trait(trait_, [ty], ParamEnv::empty()) as u8);
            answers.push(infcx.take_next_solver_overflows().len() as u8);
        }
    }
    answers.into()
}

#[test]
fn the_shape_table_answers_only_within_the_depth_it_is_given() {
    let db = db_of(&near_the_limit());
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    assert_eq!(interner.recursion_limit(), 8);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let cx = <&SolverContext<'_>>::from(&infcx);
    let names = names(&db, krate);
    let send = names.traits["Send"];
    for i in 0..CHAIN {
        let ty = Ty::new_adt(interner, names.adts[&format!("C{i}")], GenericArgs::empty());
        let needs = CHAIN - i;
        for available in [0, 5, 8, 16, 128] {
            let expected = (needs <= available).then_some(needs);
            let found = shape::auto_trait_holds(cx, send.into(), ty, ParamEnv::empty(), available);
            assert_eq!(found, expected, "C{i}: Send with {available} levels available");
        }
    }
    let u8 = interner.default_types().types.u8;
    assert_eq!(shape::auto_trait_holds(cx, send.into(), u8, ParamEnv::empty(), 0), Some(0));
}

#[test]
fn the_tables_agree_with_the_plain_solver_near_the_recursion_limit() {
    let fixture = near_the_limit();
    let plain_db = db_of(&fixture);
    let plain = switch::without(|| limit_answers(&plain_db, local_crate(&plain_db)).to_vec());
    let db = db_of(&fixture);
    assert_eq!(limit_answers(&db, local_crate(&db)), &plain[..], "{fixture}");
    // Each goal gives four bytes: (answer, warnings) twice.
    let ok = EvaluationResult::EvaluatedToOk as u8;
    let warned_ok = plain.chunks(4).filter(|it| it[0] == ok && it[1] > 0).count();
    let failed = plain.chunks(4).filter(|it| it[0] != ok).count();
    let quiet_ok = plain.chunks(4).filter(|it| it[0] == ok && it[1] == 0).count();
    assert!(warned_ok > 0 && failed > 0 && quiet_ok > 0, "{plain:?}");
}

#[test]
fn a_key_names_its_goal() {
    let db = db_of(PRELUDE);
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    let names = names(&db, krate);
    let list = Ty::new_adt(interner, names.adts["List"], GenericArgs::empty());
    let predicate: Predicate<'_> =
        TraitRef::new(interner, names.traits["Send"].into(), [list]).upcast(interner);
    let env = db.trait_environment(names.functions["shadowed"].into());
    let empty = SolverDefIds::default;
    let modes = [
        TypingMode::Coherence,
        TypingMode::Typeck { defining_opaque_types_and_generators: empty() },
        TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: empty() },
        TypingMode::PostBorrowck { defined_opaque_types: empty() },
        TypingMode::Reflection,
        TypingMode::PostAnalysis,
        TypingMode::Codegen,
    ];
    for mode in modes {
        let key = GoalKey::of(predicate, ParamEnv::empty(), mode).expect("a global goal");
        assert_eq!(key.bits() & GoalKey::PENDING, 0);
        assert_eq!(key.predicate(), predicate);
        assert_eq!(key.param_env(), ParamEnv::empty());
        assert_eq!(format!("{:?}", key.typing_mode()), format!("{mode:?}"));
    }
    // An env with a region parameter is not global.
    assert_eq!(GoalKey::of(predicate, env, TypingMode::PostAnalysis), None);
    let defining = SolverDefIds::new_from_slice(&[names.functions["shadowed"].into()]);
    let defining = TypingMode::Typeck { defining_opaque_types_and_generators: defining };
    assert_eq!(mode_tag(defining), None);
    assert_eq!(mode_tag(TypingMode::ErasedNotCoherence(MayBeErased)), None);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let var = infcx.next_ty_var(Span::Dummy);
    let open: Predicate<'_> =
        TraitRef::new(interner, names.traits["Send"].into(), [var]).upcast(interner);
    assert_eq!(GoalKey::of(open, ParamEnv::empty(), TypingMode::PostAnalysis), None);
}
