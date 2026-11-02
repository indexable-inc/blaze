//! `Devirtualize` over fixtures, through the optimized-body query a build asks, against
//! worlds made here as the code generator makes them (`hir-ty-codegen` `v2/devirt.rs`): a
//! closed object of one type is a direct call, one of three is compared, an open one stays
//! virtual saying why under the pass's span, and what a body read is listed. An integration
//! test (`tests/mir/main.rs` names this file).

use std::fmt::Write as _;

use blaze_ids::ContentId;
use hir_def::{AdtId, ModuleDefId, TraitId, signatures::{StructSignature, TraitSignature}};
use hir_ty::{
    TyDefId,
    db::HirDatabase,
    next_solver::{GenericArgs, TyKind},
};
use hir_ty_mir::mir::{
    MirBody, MirOptLevel, MirOptions, TerminatorKind,
    devirt::{self, ABSENT, Held, Object, ObjectKey, Target, World},
    print_mir_body,
};
use hir_expand::fixture;
use parking_lot::Mutex;
use rustc_type_ir::inherent::IntoKind as _;

use crate::{db::TestDB, function};

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

/// How an object of the fixture is in the world.
#[derive(Clone, Copy)]
enum Made {
    Closed,
    Open(&'static str),
}

/// The world of the fixture's objects `objects` (trait name, its types, how), given while
/// the result is held.
fn world(db: &TestDB, objects: &[(&str, &[&str], Made)]) -> Held {
    let krate = fixture::test_crate(db);
    let declared = hir_def::test_items::declarations(db, krate);
    let trait_named = |name: &str| -> TraitId {
        (declared.iter())
            .find_map(|it| match it {
                ModuleDefId::TraitId(id) if TraitSignature::of(db, *id).name.as_str() == name => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no trait `{name}`"))
    };
    let struct_named = |name: &str| -> AdtId {
        (declared.iter())
            .find_map(|it| match it {
                ModuleDefId::AdtId(AdtId::StructId(id)) if StructSignature::of(db, *id).name.as_str() == name => {
                    Some(AdtId::StructId(*id))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no struct `{name}`"))
    };
    let mut made: Vec<Object> = (objects.iter().enumerate())
        .map(|(at, &(name, tys, how))| {
            let key = ObjectKey { trait_: trait_named(name), args: GenericArgs::new_from_slice(&[]).store() };
            let targets: Box<[Target]> = (tys.iter().enumerate())
                .map(|(n, ty)| Target {
                    ty: db.ty(TyDefId::AdtId(struct_named(ty))).skip_binder().store(),
                    id: ContentId([(at * 16 + n + 1) as u8; 32]),
                    count: None,
                })
                .collect();
            let open = match how {
                Made::Closed => None,
                Made::Open(why) => Some(why),
            };
            let trait_id = ContentId([0xa0 + at as u8; 32]);
            let digest = devirt::object_digest(&trait_id, &ContentId([0; 32]), &targets, open);
            Object { key, name: format!("dyn {name}").into(), targets, open: open.map(Into::into), digest }
        })
        .collect();
    made.sort_unstable_by(|a, b| a.digest.cmp(&b.digest));
    devirt::give(World::new("the executable of crate test".into(), made).expect("one object a key"))
}

/// `check` of the body of `name` optimized against the world of `objects`, with the
/// events `Devirtualize` logged, each as `span: field=value ...`.
#[track_caller]
fn devirtualized<T>(
    name: &str,
    objects: &[(&str, &[&str], Made)],
    check: impl FnOnce(&TestDB, &MirBody<'_>, &Object, &[String]) -> T,
) -> T {
    let fx = fixture::db(SHAPES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let held = world(&db, objects);
        let options = MirOptions {
            level: MirOptLevel::Release,
            ub_checks: false,
            lifetime_markers: false,
            inline: true,
            heap_to_stack: false,
            world: Some(held.world().id),
        };
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
            Some(TerminatorKind::Call { func, .. }) => Some(func.fn_def().is_some_and(|(_, args)| {
                args.as_slice().first().and_then(|it| it.ty()).is_some_and(|it| matches!(it.kind(), TyKind::Dynamic(..)))
            })),
            _ => None,
        })
        .collect()
}

#[test]
fn one_type_closed_is_called_directly() {
    devirtualized("one", &[("Shape", &["Circle"], Made::Closed)], |_, body, object, events| {
        assert!(!calls(body).contains(&true), "a `dyn` call is left: {events:?}");
        assert_eq!(body.inlined.devirtualized, [object.digest.0], "{events:?}");
        assert!(events.iter().any(|it| it.starts_with("devirtualize:") && it.contains("outcome=\"direct\"")), "{events:?}");
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
        assert!(events.iter().any(|it| it.contains("outcome=\"compared\"") && it.contains("targets=3")), "{events:?}");
    });
}

#[test]
fn open_object_stays_virtual_saying_why() {
    const WHY: &str = "the program is a cdylib: a program that loads it may make objects of its traits";
    devirtualized("one", &[("Shape", &["Circle"], Made::Open(WHY))], |_, body, object, events| {
        assert_eq!(calls(body), [true]);
        assert_eq!(body.inlined.devirtualized, [object.digest.0]);
        let kept = events.iter().find(|it| it.contains("outcome=\"kept\"")).expect("the kept call's event");
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
        devirtualized("three", &[("Named", &["Circle", "Square", "Line"], Made::Closed)], |db, body, _, _| {
            print_mir_body(db, body, "three")
        })
    };
    assert_eq!(print(), print());
}

/// A subscriber that keeps each event with the name of the span it is in.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<Mutex<Captured>>);

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
