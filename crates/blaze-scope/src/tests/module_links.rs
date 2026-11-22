//! Names at module grain (D116): a module's records hold links, never copies, and a lookup
//! through a link reads one cell of each record it passes.

use std::boxed::Box;
use std::collections::BTreeMap;
use std::format;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CellSeen, ImportsView, LinksView, Table};

use super::{N, V, World};
use crate::links::{CellRead, Hop, LinkTables, Record, TextualHop, links_agree, lookup, lookup_textual, module_records};
use crate::{Def, Edition, Namespace, Visibility};

/// The tests' allocator: the test heap's, named here.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// One module's records as plain bytes.
struct Bytes {
    outline: Vec<u8>,
    made: Option<Vec<u8>>,
    imports: Vec<u8>,
    textual: Option<Vec<u8>>,
    exported: Option<Vec<u8>>,
}

/// Each module's records, each module's chain, and the crate.
struct Held {
    records: BTreeMap<ItemKey, Bytes>,
    chains: BTreeMap<ItemKey, Vec<ItemKey>>,
    krate: CrateItemKey,
}

struct At<'b>(&'b Held);

impl<'b> LinkTables<'b> for At<'b> {
    fn table(&self, module: ItemKey, table: Table) -> Option<LinksView<'b>> {
        let r = self.0.records.get(&module)?;
        let bytes: &'b [u8] = match table {
            Table::Outline => &r.outline,
            Table::Made => r.made.as_deref()?,
            Table::Textual => r.textual.as_deref()?,
            Table::Exported => r.exported.as_deref()?,
        };
        Some(LinksView::open(bytes).expect("a record the test wrote opens"))
    }

    fn imports(&self, module: ItemKey) -> Option<ImportsView<'b>> {
        let r = self.0.records.get(&module)?;
        Some(ImportsView::open(&r.imports).expect("a record the test wrote opens"))
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.0.chains.get(&module).map(|c| c.as_slice())
    }

    fn krate(&self) -> CrateItemKey {
        self.0.krate
    }
}

/// The records of the crate under `root`, and each module's chain.
fn held(w: &World, root: ItemKey) -> Held {
    let mut records = BTreeMap::new();
    let owned = |b: &Option<blaze_surface::RecordBytes<'_>>| b.as_ref().map(|b| b.to_vec());
    module_records(w, root, heap(), &|_: ItemKey, _: Def| {}, &mut |r| {
        let bytes = Bytes { outline: r.outline.to_vec(), made: owned(&r.made), imports: r.imports.to_vec(), textual: owned(&r.textual), exported: owned(&r.exported) };
        records.insert(r.module, bytes);
    })
    .expect("the crate's names are solved and written");
    let mut chains = BTreeMap::new();
    for &m in w.modules.keys() {
        let mut chain = Vec::new();
        let mut at = Some(m);
        while let Some(k) = at {
            chain.push(k);
            at = w.modules[&k].parent;
        }
        chains.insert(m, chain);
    }
    Held { records, chains, krate: w.modules[&root].krate }
}

fn leak(s: std::string::String) -> N {
    Box::leak(s.into_boxed_str())
}

/// A crate whose root has `root_names` functions, and its module `a` with 1,000 of its own and
/// `use super::*;`. `a`'s keys are made first, so they are the same at every `root_names`.
fn globbing_world(root_names: usize) -> (World, ItemKey, ItemKey) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    for i in 0..1000 {
        w.fun(a, leak(format!("own_{i}")), V::Pub);
    }
    w.glob(a, "super", V::Priv);
    w.fun(root, "only_in_root", V::Pub);
    for i in 0..root_names {
        w.fun(root, leak(format!("root_{i}")), V::Pub);
    }
    (w, root, a)
}

/// Should print: `test tests::module_links::a_glob_is_a_link_not_a_copy ... ok`
#[test]
fn a_glob_is_a_link_not_a_copy() {
    let (small, small_root, a) = globbing_world(1);
    let (large, large_root, a2) = globbing_world(5000);
    assert_eq!(a, a2);
    let small = held(&small, small_root);
    let large = held(&large, large_root);
    let (s, l) = (&small.records[&a], &large.records[&a]);
    assert!(s.made.is_none(), "a module with no macro call has no ModuleScope");
    assert_eq!(s.outline, l.outline, "a's outline does not grow with the module it globs");
    assert_eq!(s.imports, l.imports, "a's Imports does not grow with the module it globs");
    assert!(large.records[&large_root].outline.len() > small.records[&small_root].outline.len() + 4000 * 100);
}

/// Should print: `test tests::module_links::a_lookup_through_a_glob_reads_one_cell_of_each ... ok`
#[test]
fn a_lookup_through_a_glob_reads_one_cell_of_each() {
    let (w, root, a) = globbing_world(10);
    let h = held(&w, root);
    let mut log: AVec<'_, CellRead> = AVec::new_in(heap());
    let mut stack: AVec<'_, (ItemKey, crate::cell_key::CellKey)> = AVec::new_in(heap());
    let hop = lookup(&At(&h), a, Namespace::Values, "only_in_root", &mut log, &mut stack).expect("the walk ends");
    assert!(matches!(hop, Hop::Found(..)), "{hop:?}");
    assert_eq!(log.len(), 2, "{:?}", &log[..]);
    assert_eq!((log[0].module, log[0].record), (a, Record::Links(Table::Outline)));
    assert!(matches!(log[0].seen, CellSeen::Is(_)), "a miss in a table with globs sees the glob links");
    assert_eq!((log[1].module, log[1].record), (root, Record::Links(Table::Outline)));
    assert!(stack.is_empty());
}

/// Should print: `test tests::module_links::a_reexported_name_is_written_once ... ok`
#[test]
fn a_reexported_name_is_written_once() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let inner = w.module(root, "inner", V::Pub);
    w.glob(root, "self::inner", V::Pub);
    let names: Vec<N> = (0..20).map(|i| leak(format!("reexported_name_{i:03}"))).collect();
    for &n in names.iter() {
        w.fun(inner, n, V::Pub);
    }
    let h = held(&w, root);
    assert_eq!(h.records.len(), 2);
    for &n in names.iter() {
        let mut times = 0;
        for r in h.records.values() {
            for bytes in [Some(&r.outline), r.made.as_ref(), Some(&r.imports), r.textual.as_ref(), r.exported.as_ref()].into_iter().flatten() {
                times += bytes.windows(n.len()).filter(|w| *w == n.as_bytes()).count();
            }
        }
        assert_eq!(times, 1, "`{n}` is written {times} times across the crate's names records");
    }
    let mut log: AVec<'_, CellRead> = AVec::new_in(heap());
    let mut stack: AVec<'_, (ItemKey, crate::cell_key::CellKey)> = AVec::new_in(heap());
    let hop = lookup(&At(&h), root, Namespace::Values, names[7], &mut log, &mut stack).expect("the walk ends");
    assert!(matches!(hop, Hop::Found(..)), "{hop:?}");
}

fn ask(h: &Held, module: ItemKey, ns: Namespace, name: &str) -> (Hop, Vec<CellRead>) {
    let mut log: AVec<'_, CellRead> = AVec::new_in(heap());
    let mut stack: AVec<'_, (ItemKey, crate::cell_key::CellKey)> = AVec::new_in(heap());
    let hop = lookup(&At(h), module, ns, name, &mut log, &mut stack).expect("the walk ends");
    (hop, log.iter().copied().collect())
}

/// The equality harness on `w`'s crate under `root`: every name agrees.
fn agrees(w: &World, root: ItemKey) -> crate::links::Agreed {
    w.expanded.borrow_mut().clear();
    let expand = |call: ItemKey, _: Def| {
        w.expanded.borrow_mut().insert(call);
    };
    match links_agree(w, root, heap(), &expand) {
        Ok(a) => {
            assert_eq!(a.agreed + a.unfollowed_total(), a.names, "{a}");
            a
        }
        Err(d) => panic!("{d}"),
    }
}

/// `pub(crate) use m::*;` passes `m`'s `pub` names on as `pub(crate)`, and a private name of
/// `m` not at all.
///
/// Should print: `test tests::module_links::a_glob_narrows_to_its_visibility ... ok`
#[test]
fn a_glob_narrows_to_its_visibility() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m = w.module(root, "m", V::Pub);
    w.fun(m, "open", V::Pub);
    w.fun(m, "hidden", V::Priv);
    let a = w.module(root, "a", V::Pub);
    w.glob(a, "super::m", V::Crate);
    let h = held(&w, root);
    let (hop, _) = ask(&h, a, Namespace::Values, "open");
    let Hop::Found(p) = hop else { panic!("{hop:?}") };
    assert_eq!(p.vis, Visibility::Crate(w.modules[&root].krate));
    assert_eq!(ask(&h, a, Namespace::Values, "hidden").0, Hop::NotInTables);
    agrees(&w, root);
}

/// A `macro_rules!` is seen in a child module whose `mod` line is after it, not before; the
/// lookup reads the child's table (none), then the parent's mark row and its name row.
///
/// Should print: `test tests::module_links::a_textual_macro_reaches_children_after_its_line ... ok`
#[test]
fn a_textual_macro_reaches_children_after_its_line() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let before = w.rules(root, "before");
    let child = w.module(root, "child", V::Priv);
    w.rules(root, "after");
    let h = held(&w, root);
    let mut log: AVec<'_, CellRead> = AVec::new_in(heap());
    let hop = lookup_textual(&At(&h), child, "before", &mut log).expect("the walk ends");
    assert!(matches!(hop, TextualHop::Found(d, 0) if d.key == before), "{hop:?}");
    assert!(log.iter().all(|r| r.record == Record::Links(Table::Textual)), "{:?}", &log[..]);
    log.clear();
    assert_eq!(lookup_textual(&At(&h), child, "after", &mut log).expect("the walk ends"), TextualHop::NotInTables);
    assert!(matches!(lookup_textual(&At(&h), root, "after", &mut log).expect("the walk ends"), TextualHop::Found(..)));
    agrees(&w, root);
}

/// A `macro_rules!` defined both before and after a child's `mod` line: the child sees the one
/// before, read from the parent's cut row, one cell after the name's row; the parent sees the
/// one after.
///
/// Should print: `test tests::module_links::a_textual_macro_redefined_after_a_mod_line_is_cut ... ok`
#[test]
fn a_textual_macro_redefined_after_a_mod_line_is_cut() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let before = w.rules(root, "mx");
    let child = w.module(root, "child", V::Priv);
    let after = w.rules(root, "mx");
    let h = held(&w, root);
    let mut log: AVec<'_, CellRead> = AVec::new_in(heap());
    let hop = lookup_textual(&At(&h), child, "mx", &mut log).expect("the walk ends");
    assert!(matches!(hop, TextualHop::Found(d, 0) if d.key == before), "{hop:?}");
    assert_eq!(log.last().map(|r| r.cell), Some(crate::cell_key::textual_cut_key(child, "mx")), "{:?}", &log[..]);
    log.clear();
    let hop = lookup_textual(&At(&h), root, "mx", &mut log).expect("the walk ends");
    assert!(matches!(hop, TextualHop::Found(d, 0) if d.key == after), "{hop:?}");
    agrees(&w, root);
}

/// A `#[macro_export]` macro of a child module is a row of the root's `Exported` table, read
/// after the root's outline, and not a row of the child's outline.
///
/// Should print: `test tests::module_links::a_macro_export_is_the_roots_exported_table ... ok`
#[test]
fn a_macro_export_is_the_roots_exported_table() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let child = w.module(root, "child", V::Pub);
    let exp = w.rules_with(child, "exported_one", true);
    let h = held(&w, root);
    assert!(h.records[&root].exported.is_some());
    let (hop, log) = ask(&h, root, Namespace::Macros, "exported_one");
    assert!(matches!(hop, Hop::Found(p) if p.def.key == exp && p.vis == Visibility::Public), "{hop:?}");
    assert_eq!(log.last().map(|r| r.record), Some(Record::Links(Table::Exported)));
    let child_outline = LinksView::open(&h.records[&child].outline).expect("opens");
    let cell = crate::cell_key::cell_key(Namespace::Macros, "exported_one");
    assert!(child_outline.get(cell.0).expect("reads").is_none());
    agrees(&w, root);
}

/// The harness agrees on every fixture crate the tests above build.
///
/// Should print: `test tests::module_links::links_agree_on_the_fixture_crates ... ok`
#[test]
fn links_agree_on_the_fixture_crates() {
    for n in [0, 1, 10, 200] {
        let (w, root, _) = globbing_world(n);
        let a = agrees(&w, root);
        assert!(a.names >= 1000, "{a}");
    }
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let inner = w.module(root, "inner", V::Pub);
    w.glob(root, "self::inner", V::Pub);
    for i in 0..20 {
        w.fun(inner, leak(format!("reexported_name_{i:03}")), V::Pub);
    }
    w.use_(root, "self::inner::reexported_name_003", V::Crate);
    w.use_as(inner, "super::inner::reexported_name_004", "renamed", V::Pub);
    agrees(&w, root);
}

/// A crate with a dependency (drawn seed 8: `macro_rules! mk; mk!() /* mod m1; */ use m1::*;
/// use crate::z as p; struct z;`, `m1` also the dependency's name and a prelude module): run
/// alone, the crate's reads of its dependency stay unknown and its written scope is no answer
/// (the check refuses it, `Disagreement::Unknown`, where it once compared `p` as absent); solved
/// after its dependency, as the command solves it, every name agrees.
///
/// Should print: `test tests::module_links::a_crate_is_judged_after_its_dependencies ... ok`
#[test]
fn a_crate_is_judged_after_its_dependencies() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.dep(root, "m1", dep);
    let std = w.root(9, Edition::E2021);
    let prelude = w.module(std, "prelude", V::Pub);
    let p1 = w.module(prelude, "m1", V::Pub);
    w.item(p1, "y", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    w.rules(root, "mk");
    let call = w.call(root, "mk", crate::CallShape::FnLike);
    w.module(call, "m1", V::Priv);
    w.glob(root, "m1", V::Priv);
    w.use_as(root, "crate::z", "p", V::Priv);
    w.item(root, "z", Namespace::Types, crate::DefKind::Other, V::Priv);
    w.expanded.borrow_mut().clear();
    let expand = |call: ItemKey, _: Def| {
        w.expanded.borrow_mut().insert(call);
    };
    let alone = links_agree(&w, root, heap(), &expand);
    assert!(matches!(alone, Err(crate::links::Disagreement::Unknown { module, .. }) if module == root), "{:?}", alone.map(|a| a.names));
    match crate::cells::links_agree_world(&w, root) {
        Ok(a) => assert_eq!(a.agreed + a.unfollowed_total(), a.names, "{a}"),
        Err(d) => panic!("{d}"),
    }
}

/// The drawn crate 1, one line a module or call: what a disagreement is read against.
fn drawn(w: &World) -> std::string::String {
    use std::fmt::Write;
    let id = |k: ItemKey| u16::from_be_bytes([k.0.0[9], k.0.0[10]]);
    let mut out = std::string::String::new();
    for (&k, d) in w.decls.iter().filter(|(k, _)| k.0.0[0] == 1) {
        let what = match (w.modules.get(&k), w.owner.get(&k)) {
            (Some(m), _) => format!("mod {} (parent {:?})", m.name.unwrap_or("crate"), m.parent.map(id)),
            (None, Some(&o)) => format!("call in {}", id(o)),
            (None, None) => std::string::String::from("?"),
        };
        let _ = write!(out, "  #{} {what}:", id(k));
        for i in d.items.iter() {
            let _ = write!(out, " item {:?} {} #{} {:?};", i.ns, i.name, id(i.def.key), i.vis);
        }
        for m in d.mods.iter() {
            let _ = write!(out, " mod {} #{} {:?} use={} @{};", m.name, id(m.module), m.vis, m.macro_use, m.pos);
        }
        for u in d.uses.iter() {
            let _ = write!(out, " use {:?}{:?} {:?} {:?} @{};", u.path.kind, &u.path.segments[..], u.leaf, u.vis, u.pos);
        }
        for r in d.macro_rules.iter() {
            let _ = write!(out, " rules {} #{} @{};", r.name, id(r.def), r.pos);
        }
        for c in d.calls.iter() {
            let _ = write!(out, " call {:?} #{} @{};", &c.path.segments[..], id(c.call), c.pos);
        }
        out.push('\n');
    }
    out
}

/// The harness agrees on drawn crates: the reference's worlds (a dependency and a prelude
/// beside the crate, solved first), with macro calls.
///
/// Should print: `test tests::module_links::links_agree_on_drawn_crates ... ok`
#[test]
fn links_agree_on_drawn_crates() {
    use blaze_sim::{Broke, Config};
    blaze_sim::check(Config::cases(500), |tape| {
        let (w, root) = super::oracle::reference_world(tape, true, true);
        match crate::cells::links_agree_world(&w, root) {
            Ok(a) if a.agreed + a.unfollowed_total() == a.names => Ok(()),
            Ok(a) => Err(Broke::Oracle(format!("counts do not add up: {a}"))),
            // A drawn crate the core refuses, or leaves an import or call of unknown after its
            // dependencies are solved (GAP G8, oracle.rs `a_wide_cold_fill_equals_the_plain_
            // reference`, ignored), is the core's tests' case: no decided scope to judge by.
            Err(crate::links::Disagreement::Unsolved(_) | crate::links::Disagreement::Unknown { .. }) => Ok(()),
            Err(d) => Err(Broke::Oracle(format!("{d}\n{}", drawn(&w)))),
        }
    });
}

/// Every name of the crate rooted at `root` agrees, solved after its dependencies; the
/// disagreement is the panic's text.
fn agrees_world(w: &World, root: ItemKey) -> crate::links::Agreed {
    match crate::cells::links_agree_world(w, root) {
        Ok(a) => {
            assert_eq!(a.agreed + a.unfollowed_total(), a.names, "{a}");
            a
        }
        Err(d) => panic!("{d}"),
    }
}

/// The 23 of tempo (21 Nov 05:33, oracle.out), kind 1 of 4: a `#[macro_export]` in the own text
/// of a module whose `mod` item an expansion wrote (commonware-utils `stability_scope!(ALPHA, ..
/// { pub mod hostname; })` with `#[macro_export] macro_rules! hostname` in `hostname.rs`; the
/// same in commonware-codec `mode`, -formatting `hex`, -runtime `spawn_cell`, nix
/// `getsockopt_impl` under `feature! { pub mod socket; }`). The Reference, "Macros By Example",
/// "Path-based scope": "macros labeled with `#[macro_export]` are always `pub` and can be
/// referred to by other crates, either by path or by `#[macro_use]`"; the crate places them in
/// its root. The records' `Exported` table held it; the old answer, rebuilt by the harness from
/// the written scopes' expansion-written exports alone, missed what resolve.rs publishes as
/// `in_made` (cells/agree.rs). The twin: the same export in a module of the crate's own text,
/// in the root's own scope on both sides.
///
/// Should print: `test tests::module_links::an_export_in_a_module_an_expansion_declared_agrees ... ok`
#[test]
fn an_export_in_a_module_an_expansion_declared_agrees() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let call = w.call(root, "mk", crate::CallShape::FnLike);
    let p = w.module(call, "p", V::Pub);
    w.rules_with(p, "made_export", true);
    let a = agrees_world(&w, root);
    assert!(a.names > 0, "{a}");
    // The twin: a module of the crate's own text.
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let q = w.module(root, "q", V::Pub);
    w.rules_with(q, "own_export", true);
    agrees_world(&w, root);
}

/// Kind 2 of 4: a written `super` or `self` in a block's module (h2 0.4.20 streams.rs `use
/// super::stream::ContentLength;` in `fn send_request`; tokio chan.rs `use super::block::Read;`;
/// serde_core ser/impls.rs `use super::SerializeStruct`; alloy's `TransactionEnvelope` derive
/// writing `const _: () = { use super::*; .. }`, in alloy-consensus and tempo-primitives). The
/// Reference, `paths.qualifiers.super`: "`super` in a path resolves to the parent module"; a
/// block is an anonymous scope, no module, so `super` from a block is the parent of the module
/// around the body. The rows held the path as written and the lookup counted the blocks
/// (cells/made.rs `table_kind`). The block's own lexical `use super::*;` (its first import)
/// still reaches its parent: `OnlyInStreams` is asked through it. The twin: a named module in
/// the block's place, where `super` is that module's parent and one more `super` is needed.
///
/// Should print: `test tests::module_links::super_in_a_block_passes_over_the_blocks ... ok`
#[test]
fn super_in_a_block_passes_over_the_blocks() {
    let build = |inner: N, content_length: &'static str| {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let stream = w.module(root, "stream", V::Priv);
        w.item(stream, "ContentLength", Namespace::Types, crate::DefKind::Enum, V::Pub);
        let streams = w.module(root, "streams", V::Priv);
        w.item(streams, "OnlyInStreams", Namespace::Types, crate::DefKind::Other, V::Pub);
        let block = w.module(streams, inner, V::Priv);
        w.glob(block, "super", V::Priv);
        w.use_(block, content_length, V::Priv);
        let nested = w.module(block, "__blaze_block_1", V::Priv);
        w.glob(nested, "super", V::Priv);
        w.use_as(nested, "self::OnlyInStreams", "SelfFromNested", V::Priv);
        (w, root)
    };
    let (w, root) = build("__blaze_block_0", "super::stream::ContentLength");
    agrees_world(&w, root);
    let (w, root) = build("named", "super::super::stream::ContentLength");
    agrees_world(&w, root);
}

/// Kind 3 of 4: two imports of one name, each binding another namespace (core slice `use
/// crate::{.., range, ..};` then `pub use index::{range, ..};`; der `pub use
/// crate::{ord::ValueOrd}` then `pub use der_derive::ValueOrd;`; fixed-map `Key`; tempo
/// precompiles `Storable`, a trait and its derive). The Reference, "Use declarations" and
/// "Names", "Namespaces": a `use` binds its name in each namespace its target has, and names
/// in different namespaces may overlap. The first import's path was every namespace's row, so
/// the derive answered absent (cells/made.rs `row_import`). Both orders; the twin: one import,
/// the derive absent on both sides.
///
/// Should print: `test tests::module_links::two_imports_of_one_name_each_keep_their_namespace ... ok`
#[test]
fn two_imports_of_one_name_each_keep_their_namespace() {
    let derive = crate::DefKind::Macro(crate::MacroShapes::DERIVE);
    for trait_first in [true, false] {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let ord = w.module(root, "ord", V::Pub);
        w.item(ord, "ValueOrd", Namespace::Types, crate::DefKind::Trait, V::Pub);
        let derives = w.module(root, "derives", V::Pub);
        w.item(derives, "ValueOrd", Namespace::Macros, derive, V::Pub);
        if trait_first {
            w.use_(root, "crate::ord::ValueOrd", V::Pub);
            w.use_(root, "crate::derives::ValueOrd", V::Pub);
        } else {
            w.use_(root, "crate::derives::ValueOrd", V::Pub);
            w.use_(root, "crate::ord::ValueOrd", V::Pub);
        }
        let uint = w.module(root, "uint", V::Pub);
        w.use_(uint, "crate::ValueOrd", V::Priv);
        agrees_world(&w, root);
    }
    // The twin: the trait's import alone.
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let ord = w.module(root, "ord", V::Pub);
    w.item(ord, "ValueOrd", Namespace::Types, crate::DefKind::Trait, V::Pub);
    w.use_(root, "crate::ord::ValueOrd", V::Pub);
    agrees_world(&w, root);
}

/// Kind 4 of 4: `use crate as name;` (icu_normalizer and icu_properties baked data: `pub mod
/// icu { pub use crate as normalizer; }` in a `const _: () = { .. }` block). rustc refuses the
/// unnamed `use crate;` ("crate root imports need to be explicitly named: `use crate as
/// name;`"): renamed, it binds the crate root, a module, in the type namespace. The lookup left
/// a path of no segment to the core (`Unfollowed::EmptyPath`, links.rs `follow`). The values
/// and macro namespaces of the same import are asked too and are absent on both sides.
///
/// Should print: `test tests::module_links::use_crate_as_a_name_binds_the_root ... ok`
#[test]
fn use_crate_as_a_name_binds_the_root() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let icu = w.module(root, "icu", V::Pub);
    w.use_as(icu, "crate", "normalizer", V::Pub);
    let user = w.module(root, "user", V::Pub);
    w.use_(user, "crate::icu::normalizer::user", V::Priv);
    agrees_world(&w, root);
}
