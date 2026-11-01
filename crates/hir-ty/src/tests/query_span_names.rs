//! A query span on a body says which body: its crate and its path, so a profile can name the
//! function a slow `infer_query` was for.

use std::sync::atomic::{AtomicU64, Ordering};

use base_db::all_crates;
use hir_def::{DefWithBodyId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
use parking_lot::Mutex;

use crate::InferenceResult;

const FIXTURE: &str = r#"
//- /lib.rs crate:shapes
pub mod area {
    pub struct Square(pub u32);
    impl Square {
        pub fn side(&self) -> u32 { self.0 }
    }
    pub fn of_square(it: Square) -> u32 { it.0 * it.0 }
}
"#;

/// The values the spans named `infer_query` were given, as (field, value).
#[derive(Default)]
struct Seen {
    next: AtomicU64,
    infer: Mutex<Vec<u64>>,
    values: Mutex<Vec<(String, String)>>,
}

struct Values<'a>(&'a Mutex<Vec<(String, String)>>);

impl tracing::field::Visit for Values<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.lock().push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.lock().push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl tracing::Subscriber for &'static Seen {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if span.metadata().name() == "infer_query" {
            self.infer.lock().push(id);
        }
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, id: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        if self.infer.lock().contains(&id.into_u64()) {
            values.record(&mut Values(&self.values));
        }
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn named(db: &base_db::Db, name: &str) -> FunctionId {
    let mut found = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def {
                    found.push(it);
                }
            }
            for impl_ in module.scope(db).impls() {
                found.extend(impl_.impl_items(db).items.iter().filter_map(|(_, it)| match it {
                    hir_def::AssocItemId::FunctionId(it) => Some(*it),
                    _ => None,
                }));
            }
        }
    }
    found
        .into_iter()
        .find(|&it| FunctionSignature::of(db, it).name.as_str() == name)
        .unwrap_or_else(|| panic!("the fixture has `{name}`"))
}

/// The (field, value) pairs the `infer_query` span of `name`'s body was given.
fn infer_span_of(name: &str) -> Vec<(String, String)> {
    let db = fixture::db(FIXTURE).into_db();
    let def = DefWithBodyId::FunctionId(named(&db, name));
    let seen: &'static Seen = Box::leak(Box::default());
    tracing::subscriber::with_default(seen, || {
        InferenceResult::of(&db, def);
    });
    let mut values = seen.values.lock().clone();
    values.sort();
    values
}

fn pairs(krate: &str, function: &str) -> Vec<(String, String)> {
    vec![("function".to_owned(), function.to_owned()), ("krate".to_owned(), krate.to_owned())]
}

#[test]
fn an_infer_query_span_names_the_crate_and_path_of_its_fn() {
    assert_eq!(infer_span_of("of_square"), pairs("shapes", "shapes::area::of_square"));
}

#[test]
fn an_infer_query_span_names_a_method_under_its_impl_type() {
    assert_eq!(infer_span_of("side"), pairs("shapes", "shapes::area::<impl Square>::side"));
}
