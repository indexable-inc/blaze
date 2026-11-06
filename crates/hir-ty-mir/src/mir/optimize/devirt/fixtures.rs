//! `Devirtualize` over fixtures, through the optimized-body query a build asks, against
//! worlds made here as the code generator makes them (`hir-ty-codegen` `v2/devirt.rs`): a
//! closed object of one type is a direct call, one of three is compared, an open one stays
//! virtual saying why under the pass's span, and what a body read is listed. A type whose
//! method the body's crate can't name (an impl in a crate it does not depend on, a
//! `#[track_caller]` method behind a declaration without it) stays behind the `dyn` call, and
//! the site's digest says what was seen. An integration test (`tests/mir/main.rs` names this
//! file).

use std::fmt::Write as _;

use blaze_ids::{ContentId, Hasher};
use hir_def::{
    AdtId, FunctionId, HasModule as _, ModuleDefId, TraitId,
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::fixture;
use hir_ty::{
    TyDefId,
    db::HirDatabase,
    next_solver::{GenericArgs, TyKind},
    traits::ParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    MirBody, MirOptLevel, MirOptions, TerminatorKind,
    devirt::{self, ABSENT, Held, Object, ObjectKey, Target, World},
    interpret_mir,
    location::name_sources,
    print_mir_body,
};
use blaze_core::lock::Lock;
use rustc_type_ir::inherent::IntoKind as _;

use crate::db::TestDB;

const SHAPES: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn
pub trait Shape { fn area(&self) -> u32; }
pub trait Named { fn id(&self) -> u32; }
pub struct Circle(u32);
pub struct Square(u32);
pub struct Line(u32);
impl Shape for Circle { fn area(&self) -> u32 { self.0 * 3 } }
impl Named for Circle { fn id(&self) -> u32 { 1 } }
impl Named for Square { fn id(&self) -> u32 { 2 } }
impl Named for Line { fn id(&self) -> u32 { 3 } }
pub fn one(shape: &dyn Shape) -> u32 { shape.area() }
pub fn three(named: &dyn Named) -> u32 { named.id() }
"#;

/// A trait object made in one crate and called in another: `dep`'s `through` calls through
/// `&dyn Shape`, and `main`, which `dep` does not depend on, makes objects of its `Mine`. In
/// `dep` no impl of `Shape` for `Mine` is seen.
const ACROSS: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn
//- /dep.rs crate:dep
pub trait Shape { fn area(&self) -> u32; }
pub struct Own(pub u32);
impl Shape for Own { fn area(&self) -> u32 { self.0 } }
pub fn through(shape: &dyn Shape) -> u32 { shape.area() }
//- /main.rs crate:main deps:dep
pub struct Mine(pub u32);
impl dep::Shape for Mine { fn area(&self) -> u32 { self.0 * 2 } }
pub fn run() -> u32 { dep::through(&Mine(1)) + dep::through(&dep::Own(2)) }
"#;

/// The same with a blanket impl in `dep`: `dep` finds `Shape::area` for `main`'s `Mine`, as it
/// finds a closure's `Fn::call` (hashbrown's `rehash_in_place` calling its `&dyn Fn` hasher, a
/// closure over a member's key type), but a direct call would be an instance of `Mine` worked
/// out in `dep`, which does not load `main` and so sees none of its impls.
const BLANKET: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn
//- /dep.rs crate:dep
pub trait Shape { fn area(&self) -> u32; }
impl<T> Shape for T { fn area(&self) -> u32 { 5 } }
pub fn through(shape: &dyn Shape) -> u32 { shape.area() }
//- /main.rs crate:main deps:dep
pub struct Mine(pub u32);
pub fn run() -> u32 { dep::through(&Mine(1)) }
"#;

/// A `#[track_caller]` method whose trait's declaration is not: through `&dyn Lines` it
/// reports the location a `dyn` call gives it, never the call's.
const TRACKED: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn, panic_location
pub trait Lines { fn line(&self) -> u32; }
pub struct Mine;
impl Lines for Mine {
    #[track_caller]
    fn line(&self) -> u32 { core::panic::Location::caller().line() }
}
pub fn through(lines: &dyn Lines) -> u32 {
    lines.line()
}
pub fn run() -> u32 { through(&Mine) }
"#;

/// How an object of the fixture is in the world.
#[derive(Clone, Copy)]
enum Made {
    Closed,
    Open(&'static str),
}

/// The world of the objects `objects` (trait name, its types, how) of the fixture `text`,
/// given while the result is held. Its content IDs are of the fixture and the names, as the
/// code generator's are of the types' content: worlds are shared process-wide by ID
/// ([`devirt::give`]), and tests run in parallel, each with its own database, so two fixtures
/// must never give one ID.
fn world(db: &TestDB, text: &str, objects: &[(&str, &[&str], Made)]) -> Held {
    let fixture = Hasher::new(FIXTURE_DOMAIN).part(text.as_bytes()).finish();
    let content = |kind: &str, name: &str| {
        Hasher::new(FIXTURE_DOMAIN)
            .id(&fixture)
            .part(kind.as_bytes())
            .part(name.as_bytes())
            .finish()
    };
    let declared = declared(db);
    let trait_named = |name: &str| -> TraitId {
        (declared.iter())
            .find_map(|it| match it {
                ModuleDefId::TraitId(id) if TraitSignature::of(db, *id).name.as_str() == name => {
                    Some(*id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no trait `{name}`"))
    };
    let struct_named = |name: &str| -> AdtId {
        (declared.iter())
            .find_map(|it| match it {
                ModuleDefId::AdtId(AdtId::StructId(id))
                    if StructSignature::of(db, *id).name.as_str() == name =>
                {
                    Some(AdtId::StructId(*id))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no struct `{name}`"))
    };
    let mut made: Vec<Object> = (objects.iter())
        .map(|&(name, tys, how)| {
            let key = ObjectKey {
                trait_: trait_named(name),
                args: GenericArgs::new_from_slice(&[]).store(),
            };
            let targets: Box<[Target]> = (tys.iter())
                .map(|ty| Target {
                    ty: db.ty(TyDefId::AdtId(struct_named(ty))).skip_binder().store(),
                    id: content("type", ty),
                    count: None,
                })
                .collect();
            let open = match how {
                Made::Closed => None,
                Made::Open(why) => Some(why),
            };
            let trait_id = content("trait", name);
            let digest = devirt::object_digest(&trait_id, &ContentId([0; 32]), &targets, open);
            Object {
                key,
                name: format!("dyn {name}").into(),
                targets,
                open: open.map(Into::into),
                digest,
            }
        })
        .collect();
    made.sort_unstable_by(|a, b| a.digest.cmp(&b.digest));
    devirt::give(World::new("the executable of crate test".into(), made).expect("one object a key"))
        .unwrap_or_else(|why| panic!("the fixture's world: {why}"))
}

const FIXTURE_DOMAIN: &str = "blaze v2 devirt fixture 1";

/// Every item declared in the fixture's crates but the language's, crate by crate.
fn declared(db: &TestDB) -> Vec<ModuleDefId> {
    (base_db::all_crates(db).iter())
        .filter(|krate| !krate.data(db).origin.is_lang())
        .flat_map(|krate| hir_def::test_items::declarations(db, *krate))
        .collect()
}

/// The one function named `name` in the fixture's crates.
#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = (declared(db).into_iter())
        .filter_map(|it| match it {
            ModuleDefId::FunctionId(id) if FunctionSignature::of(db, id).name.as_str() == name => {
                Some(id)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

/// The options of a body built against the world `held`.
fn against(held: &Held) -> MirOptions {
    MirOptions {
        level: MirOptLevel::Release,
        ub_checks: false,
        lifetime_markers: false,
        inline: true,
        heap_to_stack: false,
        world: Some(held.world().id),
    }
}

/// `check` of the body of `name` optimized against the world of `objects`, with the
/// events `Devirtualize` logged, each as `span: field=value ...`.
#[track_caller]
fn devirtualized<T>(
    name: &str,
    objects: &[(&str, &[&str], Made)],
    check: impl FnOnce(&TestDB, &MirBody<'_>, &Object, &[String]) -> T,
) -> T {
    devirtualized_in(SHAPES, name, objects, check)
}

/// [`devirtualized`] over the fixture `text`.
#[track_caller]
fn devirtualized_in<T>(
    text: &str,
    name: &str,
    objects: &[(&str, &[&str], Made)],
    check: impl FnOnce(&TestDB, &MirBody<'_>, &Object, &[String]) -> T,
) -> T {
    let db = fixture::db(text).into_db();
    blaze_query::db::attach(&db, || {
        let held = world(&db, text, objects);
        let options = against(&held);
        let func = function(&db, name);
        let capture = Capture::default();
        let body = tracing::subscriber::with_default(capture.clone(), || {
            db.optimized_mir_body(func.into(), options)
                .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"))
        });
        let object = &held.world().objects()[0];
        check(&db, body, object, &capture.0.lock().events)
    })
}

/// The calls of `body`, each as whether its Self is a `dyn`.
fn calls(body: &MirBody<'_>) -> Vec<bool> {
    (body.basic_blocks.iter())
        .filter_map(|(_, block)| match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Call { func, .. }) => {
                Some(func.fn_def().is_some_and(|(_, args)| {
                    args.as_slice()
                        .first()
                        .and_then(|it| it.ty())
                        .is_some_and(|it| matches!(it.kind(), TyKind::Dynamic(..)))
                }))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn one_type_closed_is_called_directly() {
    devirtualized("one", &[("Shape", &["Circle"], Made::Closed)], |_, body, object, events| {
        assert!(!calls(body).contains(&true), "a `dyn` call is left: {events:?}");
        assert_eq!(body.inlined.devirtualized, [object.digest.0], "{events:?}");
        assert!(
            events
                .iter()
                .any(|it| it.starts_with("devirtualize:") && it.contains("outcome=\"direct\"")),
            "{events:?}"
        );
    });
}

#[test]
fn three_types_closed_are_compared_with_the_dyn_call_last() {
    let shapes: &[&str] = &["Circle", "Square", "Line"];
    devirtualized("three", &[("Named", shapes, Made::Closed)], |_, body, object, events| {
        // The vtable's address is not one a type (a lazy run's, a duplicate's): the call
        // through it stays behind the compares, and only that one.
        assert_eq!(calls(body).iter().filter(|it| **it).count(), 1, "{events:?}");
        assert!(body.inlined.devirtualized.iter().all(|it| *it == object.digest.0), "{events:?}");
        assert!(
            events.iter().any(|it| it.contains("outcome=\"compared\"") && it.contains("targets=3")),
            "{events:?}"
        );
    });
}

#[test]
fn open_object_stays_virtual_saying_why() {
    const WHY: &str =
        "the program is a cdylib: a program that loads it may make objects of its traits";
    devirtualized("one", &[("Shape", &["Circle"], Made::Open(WHY))], |_, body, object, events| {
        assert_eq!(calls(body), [true]);
        assert_eq!(body.inlined.devirtualized, [object.digest.0]);
        let kept = events
            .iter()
            .find(|it| it.contains("outcome=\"kept\""))
            .expect("the kept call's event");
        assert!(kept.starts_with("devirtualize:") && kept.contains(WHY), "{kept}");
    });
}

#[test]
fn object_the_program_never_makes_is_read_as_absent() {
    devirtualized("one", &[("Named", &["Circle"], Made::Closed)], |_, body, _, events| {
        assert_eq!(calls(body), [true]);
        assert_eq!(body.inlined.devirtualized, [ABSENT.0], "{events:?}");
    });
}

#[test]
fn devirtualized_body_is_the_same_bytes_twice() {
    let print = || {
        devirtualized(
            "three",
            &[("Named", &["Circle", "Square", "Line"], Made::Closed)],
            |db, body, _, _| print_mir_body(db, body, "three"),
        )
    };
    assert_eq!(print(), print());
}

/// The `dyn` call in `dep` of an object only `main` makes stays virtual: `dep` sees no impl
/// of `Shape` for `Mine`, so a direct call there would name a method its crate can't find.
/// The site's digest is not the object's, as what `dep` sees follows from its dependencies.
#[test]
fn a_type_of_a_crate_the_body_does_not_depend_on_stays_virtual() {
    devirtualized_in(
        ACROSS,
        "through",
        &[("Shape", &["Mine"], Made::Closed)],
        |_, body, object, events| {
            assert_eq!(calls(body), [true], "{events:?}");
            let kept = events
                .iter()
                .find(|it| it.contains("outcome=\"kept\""))
                .expect("the kept call's event");
            assert!(
                kept.contains("can't name the method") && kept.contains("finds no method"),
                "{kept}"
            );
            let unseen = devirt::site_digest(&object.digest, &[4]).0;
            assert_eq!(body.inlined.devirtualized, [unseen], "{events:?}");
            assert_ne!(unseen, object.digest.0);
        },
    );
}

/// A type of `main` stays virtual in `dep` though `dep`'s blanket impl gives its method: the
/// direct callee would be resolved in `dep`, which does not load `main`.
#[test]
fn a_type_of_a_crate_the_body_does_not_load_stays_virtual_under_a_blanket_impl() {
    devirtualized_in(
        BLANKET,
        "through",
        &[("Shape", &["Mine"], Made::Closed)],
        |_, body, object, events| {
            assert_eq!(calls(body), [true], "{events:?}");
            let kept = events
                .iter()
                .find(|it| it.contains("outcome=\"kept\""))
                .expect("the kept call's event");
            assert!(
                kept.contains("names the crate main") && kept.contains("does not load"),
                "{kept}"
            );
            assert_eq!(
                body.inlined.devirtualized,
                [devirt::site_digest(&object.digest, &[4]).0],
                "{events:?}"
            );
        },
    );
}

/// Of a type `dep` sees and one it does not, the seen one is compared and called directly and
/// the other is left to the `dyn` call past it; the digest says which was which.
#[test]
fn a_seen_type_is_compared_and_an_unseen_one_left_to_the_dyn_call() {
    devirtualized_in(
        ACROSS,
        "through",
        &[("Shape", &["Own", "Mine"], Made::Closed)],
        |_, body, object, events| {
            assert_eq!(calls(body).iter().filter(|it| **it).count(), 1, "{events:?}");
            assert!(
                events
                    .iter()
                    .any(|it| it.contains("outcome=\"compared\"") && it.contains("targets=1")),
                "{events:?}"
            );
            // `Own` then `Mine`, in the object's order: seen, unseen.
            assert_eq!(
                body.inlined.devirtualized,
                [devirt::site_digest(&object.digest, &[3, 4]).0],
                "{events:?}"
            );
        },
    );
}

/// A `#[track_caller]` method behind a declaration without it stays virtual: a direct call
/// would report the call's location, where the `dyn` call reports the method's own. The kept
/// reason names the rule, and without it the call is made direct.
#[test]
fn a_track_caller_method_behind_a_plain_declaration_stays_virtual() {
    devirtualized_in(
        TRACKED,
        "through",
        &[("Lines", &["Mine"], Made::Closed)],
        |_, body, _, events| {
            assert_eq!(calls(body), [true], "{events:?}");
            let kept = events
                .iter()
                .find(|it| it.contains("outcome=\"kept\""))
                .expect("the kept call's event");
            assert!(kept.contains("track_caller"), "{kept}");
        },
    );
}

/// Through a `dyn`, a `#[track_caller]` method whose trait's declaration is not reports the
/// line of its own definition: rustc's vtable slot for it is a `ReifyShim`, which does not
/// track its caller and calls the method at the method's `def_span`. Run, the body built
/// against the world (the call kept virtual) reports that line, as the unoptimized one does.
#[test]
fn a_dyn_call_of_a_track_caller_method_reports_its_definition() {
    let mut db = fixture::db(TRACKED).into_db();
    name_sources(&mut db, "fixture", |_, _, _| Some("main.rs".to_owned()));
    blaze_query::db::attach(&db, || {
        let held = world(&db, TRACKED, &[("Lines", &["Mine"], Made::Closed)]);
        let run = function(&db, "run");
        let env =
            ParamEnvAndCrate { param_env: db.trait_environment(run.into()), krate: run.krate(&db) }
                .store();
        let plain = db
            .monomorphized_runtime_mir_body(run.into(), GenericArgs::empty().store(), env)
            .unwrap_or_else(|e| panic!("the runtime MIR of `run`: {e:?}"));
        let optimized = db
            .optimized_mir_body(run.into(), against(&held))
            .unwrap_or_else(|e| panic!("the MIR of `run`: {e:?}"));
        // The fixture's first line after the `minicore` header is line 1 of its one file.
        let defined = 1 + TRACKED
            .lines()
            .skip_while(|it| !it.starts_with("//- minicore"))
            .skip(1)
            .position(|it| it.trim_start().starts_with("fn line(&self)"))
            .expect("the method's definition") as u32;
        assert_eq!(returned_u32(&db, plain), defined, "the unoptimized `dyn` call");
        assert_eq!(returned_u32(&db, optimized), defined, "the body built against the world");
    });
}

/// What `body`, of a function of no arguments returning a `u32`, returns under the evaluator.
fn returned_u32<'db>(db: &'db TestDB, body: &'db MirBody<'db>) -> u32 {
    let (result, _) = interpret_mir(db, body, None).unwrap_or_else(|e| panic!("{e:?}"));
    let value = result.unwrap_or_else(|e| panic!("{e:?}"));
    u32::from_le_bytes(value.memory[..4].try_into().expect("a u32's four bytes"))
}

/// A subscriber that keeps each event with the name of the span it is in.
#[derive(Clone)]
struct Capture(std::sync::Arc<Lock<Captured>>);

impl Default for Capture {
    fn default() -> Capture {
        Capture(std::sync::Arc::new(Lock::new("test.mir.devirt.captured", Captured::default())))
    }
}

#[derive(Default)]
struct Captured {
    spans: Vec<&'static str>,
    entered: Vec<u64>,
    events: Vec<String>,
}

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut captured = self.0.lock();
        captured.spans.push(span.metadata().name());
        tracing::span::Id::from_u64(captured.spans.len() as u64)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut captured = self.0.lock();
        let span = captured.entered.last().map_or("", |&id| captured.spans[id as usize - 1]);
        let mut line = format!("{span}:");
        event.record(&mut Fields(&mut line));
        captured.events.push(line);
    }

    fn enter(&self, span: &tracing::span::Id) {
        self.0.lock().entered.push(span.into_u64());
    }

    fn exit(&self, _: &tracing::span::Id) {
        self.0.lock().entered.pop();
    }
}
