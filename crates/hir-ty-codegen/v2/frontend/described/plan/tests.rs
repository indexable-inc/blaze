//! A plan's name and record, round-tripped across two databases of one source, each with a
//! planted control its oracle catches by name.

use std::alloc::Global;

use base_db::{Crate, FileId, all_crates};
use blaze_codegen::{
    backend::FrontRef,
    plan::{OptLevel, Origin, Policy},
};
use blaze_ids::{ContentId, Hasher};
use hir_expand::fixture;

use super::{Edge, NAME_DOMAIN, PlanView, ask_written, plan_body};
use crate::v2::{BuildTime, Databases, HashSource, HirFrontend, HirInstance, ItemHashes, Kind, refs};

/// `dep`'s generic (compiled by whoever asks), a plain function (compiled in `dep`), a static
/// read by a function, and `main`'s root calling each.
const PROGRAM: &str = r#"
//- minicore: sized, fn
//- /dep.rs crate:dep
pub fn id<T>(x: T) -> T { x }
pub fn plain() -> u32 { 7 }
pub static COUNT: u32 = 7;
pub fn count() -> u32 { COUNT }
//- /main.rs crate:main deps:dep
pub fn roots() {
    let _ = dep::id::<u8>(7u8);
    let _ = dep::plain();
    let _ = dep::count();
}
"#;

/// A front end over `db` with blaze items' hashes (a plan needs a stage, which a database's
/// ids give none of) whose files all have one name.
fn front<'db>(db: &'db base_db::Db, policy: &Policy) -> HirFrontend<'db> {
    let names = ItemHashes::new(Box::new(|_: FileId| Some("named".to_owned())));
    HirFrontend::new(Databases::new(db), HashSource::Items(names), BuildTime::default())
        .with_policy(policy)
}

fn crate_named(db: &base_db::Db, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// `root` and every function it reaches, each once, in the order first reached.
fn reached(frontend: &HirFrontend<'_>, root: HirInstance) -> Vec<HirInstance> {
    frontend.with(|db| {
        let mut seen = std::collections::HashSet::new();
        let mut next = vec![root];
        let mut all = Vec::new();
        while let Some(instance) = next.pop() {
            assert!(all.len() < 512, "the program reaches more than 512 functions");
            // clone: the test's set of what it reached
            if !seen.insert(instance.clone()) {
                continue;
            }
            if let Kind::Code(..) = &instance.0 {
                let refs = refs::walk(db, &instance, refs::Walking::plain(false))
                    .unwrap_or_else(|why| panic!("the walk of {instance:?}: {why}"));
                next.extend(refs.into_iter().rev().filter_map(|it| match it {
                    FrontRef::Function(callee) => Some(callee),
                    _ => None,
                }));
            }
            all.push(instance);
        }
        all
    })
}

/// The oracle of names: one name, one plan. Two plans of other bytes under one name are the
/// refusal rule 21 names, with the two lengths.
fn held_names(plans: &[(ContentId, Vec<u8>)]) -> Result<(), String> {
    for (at, (name, bytes)) in plans.iter().enumerate() {
        for (other, other_bytes) in &plans[at + 1..] {
            if name == other && bytes != other_bytes {
                return Err(format!(
                    "two plans of other bytes under one name {name:?} ({} and {} bytes): an input \
                     of the plan is missing from its name (rule 21)",
                    bytes.len(),
                    other_bytes.len()
                ));
            }
        }
    }
    Ok(())
}

/// The invariant: for every function `roots` reaches, asked for by `main`, its ask reads back in
/// a second database of the same sources as an instance whose plan has the same name and ask;
/// its plan made in either database is the same bytes; its describe read back from the plan
/// equals a describe now; each function edge is the name and ask of the callee asked for by
/// the plan's origin, and each other edge is of its reference's kind. Its control is
/// [`a_name_without_its_origin_is_caught_by_the_oracle`].
#[test]
fn a_plan_reads_back_in_a_second_database_as_the_plan_it_was_made_as() {
    let policy = crate::v2::tests::policy(OptLevel::O0);
    let (one, two) = (fixture::db(PROGRAM), fixture::db(PROGRAM));
    let (first, second) = (front(one.db(), &policy), front(two.db(), &policy));
    let main = crate_named(one.db(), "main");
    let root = first.root(hir_def::test_items::function(one.db(), main, "roots"));
    let origin = first.with(|db| first.origin(db, main));
    let all = reached(&first, root);
    assert!(all.len() >= 4, "the fixture's premise: `roots` reaches its three callees");
    for instance in all {
        let asked = (first.plan_ask(&instance, &origin, Global))
            .unwrap_or_else(|why| panic!("no ask of {instance:?}: {why:?}"));
        let (back, back_origin) = (second.ask_of(&asked.ask))
            .unwrap_or_else(|why| panic!("the ask of {instance:?} in a second database: {why}"));
        let again = second.plan_ask(&back, &back_origin, Global).expect("an ask read back has a name");
        assert_eq!(again.name, asked.name, "{instance:?}: its name in a second database");
        assert_eq!(&again.ask[..], &asked.ask[..], "{instance:?}: its ask in a second database");

        let planned = (first.plan(&asked.ask, &asked.name, Global))
            .unwrap_or_else(|why| panic!("no plan of {instance:?}: {why:?}"));
        let made_again = (second.plan(&asked.ask, &asked.name, Global))
            .unwrap_or_else(|why| panic!("no plan of {instance:?} in a second database: {why:?}"));
        assert_eq!(&planned.bytes[..], &made_again.bytes[..], "{instance:?}: two builders of one name");

        let view = PlanView::read(&planned.bytes).unwrap_or_else(|why| panic!("{instance:?}: {why}"));
        let (_, read_origin, described) =
            second.planned(&view, Global).unwrap_or_else(|why| panic!("{instance:?}: {why:?}"));
        let now = (second.with(|db| second.described(db, &back, &back_origin, None)))
            .unwrap_or_else(|why| panic!("{instance:?}: {}", why.what));
        assert_eq!(read_origin, back_origin, "{instance:?}: the plan's origin");
        assert!(described == now, "{instance:?}: the plan's describe is not a describe now");
        assert_eq!(view.len(), now.refs.len(), "{instance:?}: one edge a reference");
        for (edge, reference) in view.edges().zip(&now.refs) {
            match (edge.unwrap_or_else(|why| panic!("{instance:?}: {why}")), reference) {
                (Edge::Function { name, ask, body }, FrontRef::Function(callee)) => {
                    let it = second.plan_ask(callee, &now.origin, Global).expect("a callee's ask");
                    assert_eq!((it.name, &it.ask[..]), (name, ask), "{instance:?}: the edge of {callee:?}");
                    let again = second.with(|db| plan_body(db, callee));
                    assert_eq!(body, again, "{instance:?}: the body of the edge of {callee:?}");
                }
                (Edge::Static { .. }, FrontRef::Static(_))
                | (Edge::Extern(_), FrontRef::Extern(_))
                | (Edge::Weak(_), FrontRef::Weak(_)) => {}
                (edge, reference) => panic!("{instance:?}: the edge {edge:?} of {reference:?}"),
            }
        }
    }
}

/// The control: a name made with the origin left out (every ask written as the library's).
/// `dep::id::<u8>`, which whoever asks compiles, asked for by `main` and by the library, is
/// then two plans of other bytes under one name, which the oracle names; the real names are
/// two and pass.
#[test]
fn a_name_without_its_origin_is_caught_by_the_oracle() {
    let policy = crate::v2::tests::policy(OptLevel::O0);
    let fixture = fixture::db(PROGRAM);
    let db = fixture.db();
    let frontend = front(db, &policy);
    let (main, dep) = (crate_named(db, "main"), crate_named(db, "dep"));
    let root = frontend.root(hir_def::test_items::function(db, main, "roots"));
    let id = hir_def::test_items::function(db, dep, "id");
    let generic = (reached(&frontend, root).into_iter())
        .find(|it| it.function() == Some(id))
        .expect("`roots` calls `dep::id`");
    let main_origin = frontend.with(|db| frontend.origin(db, main));
    let planted_name = |instance: &HirInstance| {
        frontend.with(|db| {
            let (producer, stage) = frontend.record_stage().expect("a stage");
            // clone: the planted ask's instance
            let ask = ask_written(db, &(instance.clone(), Origin::Std)).expect("an ask");
            let mut hasher = Hasher::new(NAME_DOMAIN);
            hasher.id(&producer).id(&stage).update(&[0]).part(&ask);
            hasher.finish()
        })
    };
    let mut real = Vec::new();
    let mut planted = Vec::new();
    for origin in [main_origin, Origin::Std] {
        let asked = frontend.plan_ask(&generic, &origin, Global).expect("an ask");
        let plan = frontend.plan(&asked.ask, &asked.name, Global).expect("a plan");
        let bytes = plan.bytes.to_vec();
        real.push((asked.name, bytes.clone())); // clone: the bytes, under both names
        planted.push((planted_name(&generic), bytes));
    }
    assert_ne!(real[0].0, real[1].0, "one generic asked for by two origins is two plans");
    held_names(&real).unwrap_or_else(|why| panic!("{why}"));
    let why = held_names(&planted).expect_err("a name without its origin passed the oracle");
    assert!(why.contains("rule 21"), "{why}");
}

/// A static's plan: made in two databases of one source it is one name, one ask and the same
/// bytes; its ask finds the same static; its head is the describe's, and its template key read
/// off the head ([`StaticHead::shape`]) is the key the driver's build files its template under
/// (`static_template_key` of the describe). The control: the head with its value's content
/// left out of the shape keys otherwise, so the equality is not of two constants.
#[test]
fn a_static_plan_reads_back_as_its_describe_and_its_template_key() {
    use blaze_codegen::driver::{StaticShape, static_shape_key, static_template_key};
    let policy = crate::v2::tests::policy(OptLevel::O0);
    let (one, two) = (fixture::db(PROGRAM), fixture::db(PROGRAM));
    let (first, second) = (front(one.db(), &policy), front(two.db(), &policy));
    let value = crate::v2::HirStatic(hir_def::test_items::static_item(
        one.db(),
        crate_named(one.db(), "dep"),
        "COUNT",
    ));
    let asked = first.static_plan_ask(&value, Global).expect("a static's ask");
    let planned = first.static_plan(&asked.ask, &asked.name, Global).expect("a static's plan");
    let again = second.static_plan(&asked.ask, &asked.name, Global).expect("its plan, again");
    assert_eq!(&planned.bytes[..], &again.bytes[..], "two builders of one static plan");
    let view = super::StaticPlanView::read(&planned.bytes).expect("a static plan reads");
    let found = second.static_planned(&view).expect("its ask finds a static");
    let described = first.with(|db| first.described_static(db, &value)).expect("a describe");
    assert_eq!(
        second.with(|db| {
            crate::v2::Hashes::item(&second.hashes, db, blaze_items::ItemId::Static(found.0)).ok()
        }),
        Some(described.item),
        "the static the plan's ask finds"
    );
    let head = view.head;
    assert_eq!(
        (head.item, head.value, head.origin, head.mutable, head.symbol, head.name),
        (
            described.item,
            described.value,
            described.origin,
            described.mutable,
            described.symbol.as_deref(),
            described.name.as_str()
        ),
        "the head is the describe's"
    );
    assert_eq!(view.len(), described.refs.len(), "one edge a reference");
    let settings = policy.choose(&head.origin, None).expect("settings").first.settings();
    let key = static_shape_key(head.shape(), &[], &settings);
    assert_eq!(key, static_template_key(&described, &[], &settings), "one key rule for a template");
    let planted = StaticShape { value: ContentId::default(), ..head.shape() };
    assert_ne!(static_shape_key(planted, &[], &settings), key, "the control: the value is keyed");
}
