//! `one_at_a_time`: a call of a function `.clippy.toml`'s `one-at-a-time` list names, made once
//! per item of a collection, where the list names the batched form that reads them all in one
//! call. A store's single-record read in a loop pays a lookup, a system call and a wake per
//! record; the batched form sorts the records into file order and reads neighbours in one read.
//!
//! The incident (`blaze_cargo::serve::direct::slots`' doc): the walk of a unit record's lookup
//! read its slots one `serve::direct::read` at a time while `read_many` already read a batch in
//! file order; each read was correct alone, and nothing but a reader of the code named the loop
//! as the cost.
//!
//! The list: `one-at-a-time = [{ path = "blaze_cas::direct::Direct::read", batched =
//! "read_many" }]`, each entry's `path` a function from a crate's root (a trait's method names
//! every implementation's call too) and `batched` the name the finding gives. A function with no
//! batched form is not listed: a finding with no fix is noise. An entry whose path names a crate
//! the linted crate reaches but no function there is an error at the crate's root, never
//! skipped; an entry of a crate it does not depend on is not its concern.
//!
//! Per item is [`per_item`]'s: the body of a `for` loop, or a closure given to an iterator
//! adaptor's method (`map`, `for_each`, `filter_map`, `flat_map`, `try_for_each`, `fold`, ...),
//! resolved, never matched by name. A `while` or `loop` is not counted. The call's receiver or
//! an argument reads a local holding the item, so a read whose key is the same every round (one
//! to hoist, not to batch) is not this lint's. Test code is left alone.
//!
//! Its limit: it sees the calls written in a body. A query the engine calls once per item (a
//! tracked function that reads one record, asked for each of a thousand items by the engine's
//! own walk) is a read one at a time that no body writes in a loop, and this lint cannot see
//! it; the profile's spans of that query, counted per build, can.
//!
//! The accepted exception: `#[expect(blaze::one_at_a_time, reason = "..")]` on the function,
//! saying why one at a time is right there (each read decides whether the next is made).

use hir_def::{DefWithBodyId, hir::Expr};

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
    per_item::{element_bindings, mentions, per_item, written_call},
    test_code,
};
use crate::{check::Cx, conf::ONE_AT_A_TIME_KEY};

const LINT: Lint = Lint::OneAtATime;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.conf().value(ONE_AT_A_TIME_KEY).is_none() {
        return;
    }
    if cx.at_crate_root() {
        refused_entries(cx);
        return;
    }
    // A body's few findings, reported and dropped here: the global heap.
    let found = find(cx, std::alloc::Global);
    facts::report(cx, found);
}

/// The entries the crate cannot resolve, each an error at the crate's root.
fn refused_entries(cx: &mut Cx<'_, '_>) {
    let resolved = cx.disallowed();
    let root = cx.root();
    for error in resolved.one_at_a_time.errors {
        let span = cx.span(syntax::TextRange::empty(syntax::TextSize::new(0)));
        cx.blaze(LINT, root, span, format!("error reading Clippy's configuration file: {error}"))
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The findings of the body walked, in `alloc`, the caller's.
fn find<A: std::alloc::Allocator + Clone>(cx: &Cx<'_, '_>, alloc: A) -> Vec<Finding, A> {
    let mut found = Vec::new_in(alloc);
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let resolved = &cx.disallowed().one_at_a_time;
    if resolved.by_fn.is_empty() || test_code::is_test_code(cx, cx.root()) {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let (receiver, args) = match expr {
            Expr::Call { args, .. } => (None, &args[..]),
            Expr::MethodCall { receiver, args, .. } => (Some(receiver), &args[..]),
            _ => continue,
        };
        if !written_call(analysis, id) {
            continue;
        }
        let Some((f, _)) = written_callee(infer, body, id) else { continue };
        let Some(entry) = resolved.of(db, hir::Function::from(f)) else { continue };
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let Some(over) = per_item(cx, analysis, parents, id) else { continue };
        let bindings = element_bindings(body, over);
        let reads_item = receiver.into_iter().chain(args.iter().copied()).any(|it| mentions(db, def, body, it, &bindings));
        if !reads_item {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!(
            "`{}` reads one record per item of {}; use `{}`, which reads them all in one call",
            entry.path,
            over.say(db),
            entry.batched,
        );
        let note = format!(
            "each call pays its own lookup, read and wake; `{}` sorts the records into file order and reads \
             neighbours in one read (`one-at-a-time` in clippy.toml names it)",
            entry.batched
        );
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        conf::Conf,
        tests::{lint, lint_with},
    };

    const FOUND: &str = "reads one record per item of";

    const FLAGS: &str = "-A clippy::all --warn=blaze::one_at_a_time";

    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, fn, iterator, iterators, slice
//- /store/lib.rs crate:store
pub struct Key(pub u8);
pub struct Store;
impl Store {
    pub fn read(&self, key: &Key) -> Option<u8> { let _ = key; None }
    pub fn read_many(&self, keys: &[Key]) -> u8 { let _ = keys; 0 }
}
pub trait Images {
    fn get(&self, key: &Key) -> Option<u8>;
}
//- /src/lib.rs crate:main deps:store
"#;

    fn conf(text: &str) -> Conf {
        Conf::of_text(Some((PathBuf::from("clippy.toml"), text.to_owned())), None).expect("the list reads")
    }

    fn listing() -> Conf {
        conf(
            "one-at-a-time = [\n  { path = \"store::Store::read\", batched = \"read_many\" },\n  \
             { path = \"store::Images::get\", batched = \"get_many\" },\n]\n",
        )
    }

    fn found(main: &str, conf: Conf) -> String {
        lint_with(&format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n')), FLAGS, conf)
    }

    /// A listed read once per item of a `for` loop, of a closure given to `map`, and a trait's
    /// listed method called on an implementation, each naming the batched form.
    #[test]
    fn a_listed_read_per_item_is_found_with_its_batched_form() {
        let out = found(
            r#"
use store::{Images, Key, Store};
pub struct Mine;
impl Images for Mine { fn get(&self, key: &Key) -> Option<u8> { let _ = key; None } }
pub fn each(store: &Store, keys: &[Key]) {
    for key in keys {
        let _ = store.read(key);
    }
}
pub fn mapped(store: &Store, keys: &[Key]) -> Option<Option<u8>> {
    keys.iter().map(|key| store.read(key)).next()
}
pub fn through(images: &Mine, keys: &[Key]) -> Option<u8> {
    keys.iter().filter_map(|key| images.get(key)).next()
}
"#,
            listing(),
        );
        assert_eq!(out.matches(FOUND).count(), 3, "{out}");
        assert!(out.contains("`store::Store::read` reads one record per item of this `for` loop; use `read_many`"), "{out}");
        assert!(out.contains("of the closure given to `map`; use `read_many`"), "{out}");
        assert!(out.contains("`store::Images::get` reads one record per item of the closure given to `filter_map`; use `get_many`"), "{out}");
    }

    /// Not a finding: a read outside a loop, the batched form, a read whose key is the same
    /// every round, a read in a closure that is no iterator's, a function the list does not
    /// name, and a test; and nothing at all with no list.
    #[test]
    fn a_single_read_a_batch_a_hoistable_read_and_a_test_are_left_alone() {
        const MAIN: &str = r#"
use store::{Key, Store};
pub fn once(store: &Store, key: &Key) -> Option<u8> { store.read(key) }
pub fn batch(store: &Store, keys: &[Key]) -> u8 { store.read_many(keys) }
pub fn same(store: &Store, keys: &[Key], fixed: &Key) -> Option<Option<u8>> {
    keys.iter().map(|_| store.read(fixed)).next()
}
pub fn maybe(store: &Store, key: Option<&Key>) -> Option<u8> { key.and_then(|it| store.read(it)) }
#[cfg(test)]
mod tests {
    pub fn each(store: &store::Store, keys: &[store::Key]) {
        for key in keys { let _ = store.read(key); }
    }
}
"#;
        let out = found(MAIN, listing());
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        let out = lint(&format!("{}{}", CRATES.trim_start_matches('\n'), MAIN.trim_start_matches('\n')), FLAGS);
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// The fix the finding offers is the batched form's name, from the list; an `#[expect]`
    /// on the function holds it.
    #[test]
    fn the_finding_names_the_configured_fix_and_an_expectation_holds_it() {
        let out = found(
            r#"
use store::{Key, Store};
#[expect(blaze::one_at_a_time, reason = "each read decides whether the next is made")]
pub fn until(store: &Store, keys: &[Key]) -> u8 {
    for key in keys {
        if let Some(it) = store.read(key) { return it; }
    }
    0
}
"#,
            listing(),
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }

    /// An entry that does not read is the load's error; one whose crate the linted crate reaches
    /// but whose function it does not find is an error at the crate's root.
    #[test]
    fn an_entry_that_does_not_read_or_resolve_is_refused() {
        for (text, says) in [
            ("one-at-a-time = [\"store::Store::read\"]\n", "expected a table with `path` and `batched`"),
            ("one-at-a-time = [{ path = \"store::Store::read\" }]\n", "an entry with no `batched`"),
            ("one-at-a-time = [{ path = \"a::b\", batched = \"c\", why = \"d\" }]\n", "unknown field `why`"),
        ] {
            let error = Conf::of_text(Some((PathBuf::from("clippy.toml"), text.to_owned())), None).expect_err(text);
            assert!(error.contains(says), "{text}: {error}");
        }
        let out = found(
            "pub fn none() {}\n",
            conf("one-at-a-time = [\n  { path = \"store::Store::reed\", batched = \"read_many\" },\n  { path = \"elsewhere::read\", batched = \"read_many\" },\n]\n"),
        );
        assert!(out.contains("`store::Store::reed` of `one-at-a-time` names no function"), "{out}");
        assert!(!out.contains("elsewhere::read"), "a crate `main` does not depend on is not its concern: {out}");
    }
}
