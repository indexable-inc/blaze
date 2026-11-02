//! What an edit costs (design §10, §12), read off the `Work` the maintainer reports for the
//! crate; what it reruns (no table of another trait executes, and a body edit reruns no
//! table); what a read of the slices allocates (nothing); and what the maintainer hands the
//! next one ([`LastWrite`]).
//!
//! `Work`'s fields as the `impls` maintainer fills them: `ops`, the impls, derives and traits
//! of name resolution's delta; `woken`, the impls and traits a changed binding or def woke and
//! the modules whose body parts were read again; `rebuilt`, the keys and item names computed;
//! `slices`, the inputs set (directory fields included); `keys`, the keys, item-name sets and
//! body parts that changed.

use std::fmt::Write as _;

use blaze_dbsp::Work;
use hir_def::{AdtId, ModuleDefId, nameres::crate_def_map};
use hir_expand::name::Name;
use intern::Symbol;

use super::{Loaded, check, impl_keys, trait_named, work_of};
use crate::{
    method_resolution::{
        InherentImpls, SelfTyKey, TraitImpls,
        delta::{ImplsDelta, LastWrite, candidate_traits},
        in_crate_trait_value_items,
    },
    next_solver::SimplifiedType,
};

/// A crate of `count` modules: the root with `T`, `U` and an impl of each, and `count - 1`
/// file modules below it, each with a struct.
fn generated_crate(count: usize) -> String {
    generated_crate_with(count, "")
}

/// [`generated_crate`] with `last` after the struct of its last module.
fn generated_crate_with(count: usize, last: &str) -> String {
    let mut root = String::from(
        "pub trait T { fn t(&self); }\npub trait U { fn u(&self); }\npub struct R;\n\
         impl T for R { fn t(&self) {} }\nimpl U for R { fn u(&self) {} }\nimpl R {}\n",
    );
    let mut files = String::new();
    for at in 1..count {
        _ = writeln!(root, "mod m{at};");
        _ = write!(files, "//- /m{at}.rs\npub struct S;\n");
        if at == count - 1 {
            files.push_str(last);
        }
    }
    format!("//- /main.rs crate:main\n{root}{files}")
}

/// The `Work` of adding `impl crate::T for S {}` to the last module of a crate of `count`.
fn adding_an_impl(count: usize) -> (Loaded, Work) {
    let mut loaded = Loaded::new(&generated_crate(count));
    let krate = loaded.krate();
    let leaf = loaded.files[count - 1];
    let text = "pub struct S;\nimpl crate::T for S {}\n";
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, text));
    (loaded, work)
}

#[test]
fn adding_an_impl_costs_one_op_and_one_slice() {
    let (small, at_10) = adding_an_impl(10);
    let (_, at_1000) = adding_an_impl(1000);
    assert_eq!(at_10, at_1000, "the same edit in a crate 100 times larger costs the same");
    let Work { ops, woken, rebuilt, region, slices, keys, moved: _ } = at_10;
    assert_eq!((ops, slices), (1, 1), "{at_10:?}");
    let expected = (1, 1, 1, 0);
    assert_eq!((woken, rebuilt, keys, region), expected, "the leaf's bodies read again: {at_10:?}");
    let mut failures = String::new();
    check(&small.db, small.krate(), "adding an impl", &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

/// Whether `query` is among the ingredient names in `executed`, which name a method of a
/// tracked impl by its type and method (`TraitImpls::for_crate_and_trait`).
fn ran(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
}

#[test]
fn an_impl_of_one_trait_runs_no_table_of_another() {
    let mut loaded = Loaded::new(&generated_crate(10));
    let krate = loaded.krate();
    let (t, u) = (trait_named(&loaded.db, krate, "T"), trait_named(&loaded.db, krate, "U"));
    let read = |loaded: &Loaded| {
        let db = &loaded.db;
        crate::attach_db(db, || {
            (
                TraitImpls::in_crate_for_trait(db, krate, t).len(),
                TraitImpls::in_crate_for_trait(db, krate, u).len(),
            )
        })
    };
    assert_eq!(read(&loaded), (1, 1));
    let leaf = loaded.files[9];
    loaded.db.set_file_text(leaf, "pub struct S;\nimpl crate::T for S {}\n");
    let warm = loaded.db.log_executed(|| assert_eq!(read(&loaded), (2, 1)));
    assert_eq!(ran(&warm, "::for_crate_and_trait"), 1, "only `T`'s table runs: {warm:#?}");
    assert_eq!(ran(&warm, "::in_crate_for_trait"), 1, "only `T`'s reader runs: {warm:#?}");
    for query in ["::for_crate", "trait_bodies", "inherent_bodies", "body_part"] {
        assert_eq!(ran(&warm, query), 0, "{query} ran: {warm:#?}");
    }
}

/// Every table reader of a crate, warm: the crate tables, both traits' and `R`'s.
fn read_every_table(loaded: &Loaded) -> Vec<String> {
    let db = &loaded.db;
    let krate = loaded.krate();
    let (t, u) = (trait_named(db, krate, "T"), trait_named(db, krate, "U"));
    let r = root_struct(loaded);
    db.log_executed(|| {
        crate::attach_db(db, || {
            _ = TraitImpls::for_crate(db, krate);
            _ = InherentImpls::for_crate(db, krate);
            _ = TraitImpls::in_crate_for_trait(db, krate, t);
            _ = TraitImpls::in_crate_for_trait(db, krate, u);
            _ = InherentImpls::in_crate_for_self_ty(db, krate, r);
        })
    })
}

/// The `Work` of editing a body of the last module of a crate of `count` (`fn f`, whose body
/// holds `holds`), and what the readers of every table execute after it.
fn editing_a_body(count: usize, holds: &str) -> (Work, Vec<String>) {
    let before = format!("pub fn f() {{ {holds} let x = 1; }}\n");
    let mut loaded = Loaded::new(&generated_crate_with(count, &before));
    let krate = loaded.krate();
    _ = read_every_table(&loaded);
    let leaf = loaded.files[count - 1];
    let text = format!("pub struct S;\npub fn f() {{ {holds} let x = 2; }}\n");
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, &text));
    let warm = read_every_table(&loaded);
    let mut failures = String::new();
    check(&loaded.db, krate, "editing a body", &mut failures);
    assert!(failures.is_empty(), "{failures}");
    (work, warm)
}

#[test]
fn a_body_edit_in_a_large_crate_reruns_no_table() {
    let (at_10, _) = editing_a_body(10, "");
    let (at_1000, warm) = editing_a_body(1000, "");
    assert_eq!(at_10, at_1000, "the same edit in a crate 100 times larger costs the same");
    let Work { ops, woken, rebuilt, region, slices, keys, moved: _ } = at_1000;
    assert_eq!((ops, woken, rebuilt, region, slices, keys), (0, 1, 0, 0, 0, 0), "{at_1000:?}");
    for query in [
        "::for_crate",
        "::for_crate_and_trait",
        "::in_crate_for_trait",
        "::in_crate_for_self_ty",
        "trait_bodies",
        "inherent_bodies",
        "body_part",
    ] {
        assert_eq!(ran(&warm, query), 0, "{query} ran: {warm:#?}");
    }
}

/// A body that holds an impl, edited elsewhere: its module's body table reruns and is equal,
/// so no reader of it does.
#[test]
fn a_body_edit_that_keeps_its_impls_reruns_no_table() {
    let (work, warm) = editing_a_body(1000, "impl crate::T for S {}");
    assert_eq!((work.slices, work.keys), (0, 0), "{work:?}");
    for query in ["::for_crate", "::for_crate_and_trait", "::in_crate_for_trait"] {
        assert_eq!(ran(&warm, query), 0, "{query} ran: {warm:#?}");
    }
}

/// The `Work` of adding `impl crate::T for S {}` to the body of `fn f` in the last module of a
/// crate of `count`, checked against the reference.
fn adding_a_body_impl(count: usize) -> Work {
    let mut loaded = Loaded::new(&generated_crate_with(count, "pub fn f() { let x = 1; }\n"));
    let krate = loaded.krate();
    let leaf = loaded.files[count - 1];
    let text = "pub struct S;\npub fn f() { impl crate::T for S {} let x = 1; }\n";
    let work = work_of(&mut loaded.db, krate, |db| db.set_file_text(leaf, text));
    let mut failures = String::new();
    check(&loaded.db, krate, "adding a body impl", &mut failures);
    assert!(failures.is_empty(), "{failures}");
    work
}

/// A body-block impl added costs its own facts: the module's body part read again and changed,
/// and `T`'s slice set with the module among its body modules.
#[test]
fn adding_a_body_impl_costs_one_part_and_one_slice() {
    let (at_10, at_1000) = (adding_a_body_impl(10), adding_a_body_impl(1000));
    assert_eq!(at_10, at_1000, "the same edit in a crate 100 times larger costs the same");
    let Work { ops, woken, rebuilt, region, slices, keys, moved: _ } = at_1000;
    assert_eq!((ops, woken, rebuilt, region, slices, keys), (0, 1, 0, 0, 1, 1), "{at_1000:?}");
}

/// The impls maintainer's [`LastWrite`] for `krate` (none when the write left it alone).
fn last_write(loaded: &mut Loaded) -> Option<ImplsDelta> {
    let krate = loaded.krate();
    loaded.db.with_state::<LastWrite, _>(|_, last| {
        let at = last.crates.iter().position(|it| it.krate == krate)?;
        let delta = &last.crates[at];
        Some(ImplsDelta {
            krate,
            keys_changed: delta.keys_changed.clone(),
            traits_changed: delta.traits_changed.clone(),
        })
    })
}

#[test]
fn the_write_delta_names_the_impls_and_traits_that_changed() {
    let mut loaded = Loaded::new(&generated_crate(10));
    let krate = loaded.krate();
    let before = impl_keys(&loaded.db, krate);
    let leaf = loaded.files[9];
    loaded.db.set_file_text(leaf, "pub struct S;\nimpl crate::T for S {}\n");
    let added: Vec<_> =
        impl_keys(&loaded.db, krate).into_keys().filter(|it| !before.contains_key(it)).collect();
    assert_eq!(added.len(), 1);
    let expected = ImplsDelta { krate, keys_changed: added, traits_changed: Vec::new() };
    assert_eq!(last_write(&mut loaded), Some(expected));

    loaded.db.set_file_text(leaf, "pub struct S;\nimpl crate::T for S {}\npub fn f() {}\n");
    assert_eq!(last_write(&mut loaded), None, "an item that is no impl changes no key");

    let t = trait_named(&loaded.db, krate, "T");
    let root = loaded.files[0];
    let text = generated_crate(10);
    let text = text
        .split("//- /")
        .nth(1)
        .expect("the root's file")
        .trim_start_matches("main.rs crate:main\n")
        .replace("fn t(&self); }", "fn t(&self); fn t2(&self); }");
    loaded.db.set_file_text(root, &text);
    let delta = last_write(&mut loaded).expect("a trait's names changed");
    assert_eq!(delta.traits_changed, [t]);
}

/// The struct `R` of the crate root, as a self type key.
fn root_struct(loaded: &Loaded) -> SelfTyKey<'_> {
    let db = &loaded.db;
    let r = crate_def_map(db, loaded.krate())
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::AdtId(it @ AdtId::StructId(_)) => Some(it),
            _ => None,
        })
        .expect("the root's struct");
    SelfTyKey(SimplifiedType::Adt(r.into()))
}

#[test]
fn a_read_of_the_slices_allocates_nothing() {
    let loaded = Loaded::new(&generated_crate(10));
    let db = &loaded.db;
    let krate = loaded.krate();
    let t = trait_named(db, krate, "T");
    let r = root_struct(&loaded);
    let name = Name::new_symbol_root(Symbol::intern("t"));
    let read = || {
        TraitImpls::for_crate_and_trait(db, krate, t).blanket_impls(t).len()
            + TraitImpls::in_crate_for_trait(db, krate, t).len()
            + InherentImpls::in_crate_for_self_ty(db, krate, r).len()
            + in_crate_trait_value_items(db, krate, name).len()
            + candidate_traits(db, krate, name).len()
    };
    let warm = read();
    assert_eq!(warm, 4, "an impl of `T`, one of `R`, and `T` twice for `t`");
    blaze_alloc_count::assert_at_most(0, "a warm read of the slices", || {
        assert_eq!(std::hint::black_box(read()), warm);
    });
}
