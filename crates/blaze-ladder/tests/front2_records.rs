//! front2 over each crate's RECORDS answers what front2 over the names solved in this command
//! answers (front2/source.rs: the one seam, `NamesSource`; rule 78: each instantiation tested
//! equal).
//!
//! The owner's hello world is solved once; each crate's names record and items record are
//! written from that solve (`names_record::encode`, `items_record::encode`: what `Kept::names`
//! and `Kept::items` are handed); every answer of the solved names is taken as text; THE
//! SOLVED NAMES ARE DROPPED; then the same questions are asked of `Records` alone, through the
//! same generic code, and the two texts must be equal. Asked, for the owner's `main` and for
//! ten fns of blaze's own standard library chosen by key (the ten least keys): every question
//! of the port notes' table that a switched function of front2 asks (the item's row and text,
//! its crate's facts, the traits in scope, written paths through `resolve::Paths`, a trait's
//! members, an enum's variants, a variant's enum, a macro's body, `fill::want_of_item`), the
//! lang index (`Index::from_source`), `langs::future_trait`, and the impl index
//! (`Buckets::lazy` + `admit` against `Buckets::new`).
//!
//! PLANTED BUG: a second `Records` with one crate's impl rows withheld
//! (`Records::plant_no_impls`) must give a different candidate list for `bool: Clone`, and the
//! test checks it does. A third `Records` whose loader keeps no items record for core must
//! file the typed "no items record for crate `core`".
//!
//! NOT HERE YET (said by the test's last line): a body CHECKED over records. front2's head
//! store, type builder, checker world and check loop still take `&Names`; when they are
//! switched to `S: NamesSource`, this test gains `check_body` over both and compares the
//! outcomes.
//!
//! It should print (numbers are the run's):
//! `RECORDS: 11 items, every answer equal over both sources`
//! `RECORDS: first item <t>, tenth item <t>; names records opened <n> (<b> bytes), scopes decoded <n> (<b> bytes); items records opened <n> (<b> bytes), member lists <n>, texts read <n> (<b> bytes)`
//! `PLANT caught: core's impl rows withheld: bool: Clone has <n> candidates over records, <m> solved`
//! `NO RECORD: no items record for crate `core` (crate <k>)`
//! `NOT YET: no body is checked over records (Store, Builder, World, check loop take &Names)`
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs.

#![feature(allocator_api)]

use std::collections::HashMap;
use std::time::Instant;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::buckets::{Buckets, inherent_home, trait_home};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::{Keys, checker_of};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::front2::{Front2, fill, langs};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Kind, Ns, Paths};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_typeck::Shape;

/// His `Cargo.toml`'s edition line.
const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs`, as written.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

/// The lang names compared one by one (the whole set is compared by its count).
const LANGS: [&str; 8] = ["sized", "copy", "clone", "drop", "deref", "future_trait", "Option", "Result"];

/// The written paths asked in each item's module: prelude names, a variant through the
/// prelude, a variant through its enum, a builtin, the crate root, and one that names nothing.
const PATHS: [(&[&str], Ns); 8] = [
    (&["Sized"], Ns::Type),
    (&["Clone"], Ns::Type),
    (&["Result"], Ns::Type),
    (&["Ok"], Ns::Value),
    (&["Option", "Some"], Ns::Value),
    (&["u32"], Ns::Type),
    (&["crate"], Ns::Type),
    (&["no_such_name_anywhere"], Ns::Value),
];

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve: the test's effect layer.
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    /// The crate whose items record is not kept (the third reader's).
    no_items: Option<usize>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        if self.no_items == Some(krate) {
            return None;
        }
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.files.get(krate)?.get(path).copied()
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }
}

/// Every answer about `item` a switched function of front2 asks, as text. ONE function over
/// both sources: what differs between two calls is the source alone.
fn answers<'n, S: NamesSource<'n>>(names: S, item: ItemKey) -> String {
    let paths = Paths { names };
    let mut out = String::new();
    out.push_str(&format!("text {:?}\n", names.text_of(item)));
    out.push_str(&format!("name {:?} home {:?} has_text {} ctor {}\n", names.name_of(item), names.home_of(item), names.has_text(item), names.is_ctor(item)));
    out.push_str(&format!("place {:?}\n", names.place_of(item).map(|p| (p.krate, p.at, p.item))));
    out.push_str(&format!("kind {:?} detail {:?}\n", names.kind_of_item(item), names.detail_of(item)));
    out.push_str(&format!("want {}\n", match fill::want_of_item(names, item) {
        Ok(_) => "a head".to_string(),
        Err(e) => format!("{e}"),
    }));
    let krate = names.crate_of(item);
    out.push_str(&format!("crate {krate:?}\n"));
    if let Some(k) = krate {
        out.push_str(&format!(
            "crate facts: key {:?} root {:?} edition {:?} proc_macro {:?} incoherent {:?} cfg {:?}\n",
            names.crate_key(k),
            names.root_module(k),
            names.edition_of(k),
            names.proc_macro(k),
            names.may_write_incoherent(k),
            names.cfg_atoms(k)
        ));
    }
    let Some(module) = names.home_of(item) else {
        out.push_str("no module\n");
        return out;
    };
    match names.traits_in_scope(module, Alloc::global()) {
        Ok(traits) => out.push_str(&format!("traits in scope {:?}\n", &traits[..])),
        Err(e) => out.push_str(&format!("traits in scope: {e:?}\n")),
    }
    for (segments, ns) in PATHS {
        let found = paths.path(module, false, segments, ns);
        out.push_str(&format!("path {segments:?} {ns:?}: {found:?}\n"));
        let Ok(target) = found else { continue };
        match target.kind {
            Kind::Trait => {
                let mut rows = Vec::new();
                let listed = names.each_member(target.key, &mut |m| rows.push(format!("{m:?}")));
                out.push_str(&format!("  members (listed {listed}): {rows:?}\n"));
            }
            Kind::Enum => out.push_str(&format!("  variants {:?}\n", names.variants(target.key))),
            Kind::Variant { adt, .. } => {
                out.push_str(&format!("  variant of {:?} (its enum by path: {adt:?}), index {:?}\n", names.variant_of(target.key), paths.variant_index(adt, target.key)));
            }
            _ => {}
        }
    }
    for name in ["println", "vec", "no_such_macro_anywhere"] {
        out.push_str(&format!("macro {name}: {:?}\n", paths.macro_body(module, false, &[name]).map(|b| b.is_some())));
    }
    out
}

/// The lang index's answers, as text.
fn lang_answers<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>) -> String {
    let mut out = format!("lang items: {}\n", index.lang_count());
    for name in LANGS {
        out.push_str(&format!("lang {name}: {:?}\n", index.lang(name)));
    }
    if let Some(sized) = index.lang("sized") {
        out.push_str(&format!("future trait: {:?}\n", langs::future_trait(Paths { names }, sized)));
    }
    out
}

/// The impl index's answers to `bool: Clone` and to bool's inherent impls, as text, and the
/// trait question's candidates. `b` is the source's own index (`Buckets::new`, or
/// `Buckets::lazy`, which `admit` then fills for each question's home).
fn impl_answers<'n, S: NamesSource<'n>>(names: S, sized: Option<ItemKey>, frame: &'n blaze_mem::Frame, root: ItemKey, b: &mut Buckets) -> (String, usize) {
    let mut keys = Keys::new();
    let store = Store::new(sized, frame);
    let clone = Paths { names }.path(root, false, &["Clone"], Ns::Type).unwrap_or_else(|e| panic!("`Clone` from the program's root: {e}"));
    assert_eq!(clone.kind, Kind::Trait, "`Clone` names a trait");
    keys.item(clone.key).unwrap_or_else(|e| panic!("filing `Clone`: {e}"));
    let trait_ = checker_of(clone.key);
    let home = trait_home(names, &keys, &store, trait_, Shape::Bool);
    b.admit(names, home);
    let of_trait = b.trait_candidates(trait_, Shape::Bool, home);
    let inherent = inherent_home(names, &keys, Shape::Bool);
    b.admit(names, inherent);
    let of_bool = b.inherent_candidates(Shape::Bool, inherent);
    (format!("bool: Clone in {home:?}: {of_trait:?}\nimpl bool in {inherent:?}: {of_bool:?}\n"), of_trait.len())
}

/// A reader over `kept` alone: its log, its frame and itself live for the test (leaked: a
/// `Records` borrows itself for its whole life).
fn reader(keys: &'static [CrateItemKey], crate_names: &'static [&'static str], kept: Kept) -> (&'static Records<'static>, &'static blaze_mem::Frame) {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, Alloc::global())));
    let load: &'static Kept = Box::leak(Box::new(kept));
    (Box::leak(Box::new(Records::new(keys, load, log, frame, Alloc::global()))), frame)
}

#[test]
fn records_answer_what_the_solved_names_answer() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let core = position("core").expect("a crate named core");
    // The crates that can declare a lang item: the standard library's and the program's own.
    let lang_crates: Vec<usize> = [Some(0), Some(core), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();

    // The solve, the records written from it, and every solved answer as text. Nothing of the
    // solved names leaves this block.
    let g = Alloc::global();
    let make = |names: &blaze_scope::resolve::Names<'_>, no_items: Option<usize>| -> Kept {
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), no_items };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), g));
            kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), g));
            kept.files.push(c.files.iter().map(|f| (f.path.to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        }
        kept
    };
    let (kept, kept_planted, kept_no_core, chosen, root, solved_items, solved_langs, solved_impls, solved_candidates) = {
        let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
        let root = names.root(0);
        let main = Paths { names: &names }.path(root, false, &["main"], Ns::Value).unwrap_or_else(|e| panic!("`main` from his crate root: {e}")).key;
        // Ten fns of blaze's own standard library, chosen by key: the ten least.
        let library = position("bstd").or(position("blaze_std")).unwrap_or_else(|| panic!("no crate named bstd or blaze_std among {crate_names:?}"));
        let mut fns: Vec<ItemKey> = names.items(library).map(|(key, _, _)| key).filter(|key| (&names).kind_of_item(*key) == Some(ItemKind::Function)).collect();
        fns.sort();
        fns.truncate(10);
        assert_eq!(fns.len(), 10, "`{}` has ten fns", crate_names[library]);
        let mut chosen = vec![main];
        chosen.extend(fns);
        let solved_items: Vec<String> = chosen.iter().map(|k| answers(&names, *k)).collect();
        let solved_langs = lang_answers(&names, &front.index);
        let mut buckets = Buckets::new(&names, &front.index);
        let (solved_impls, solved_candidates) = impl_answers(&names, front.index.lang("sized"), &frame, root, &mut buckets);
        (make(&names, None), make(&names, None), make(&names, Some(core)), chosen, root, solved_items, solved_langs, solved_impls, solved_candidates)
    };

    // Over the records alone.
    let (records, frame) = reader(keys, crate_names, kept);
    let front = Front2::from_records(records, frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let mut times = Vec::new();
    for (i, key) in chosen.iter().enumerate() {
        let start = Instant::now();
        let got = answers(records, *key);
        times.push(start.elapsed());
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "item {i} ({key:x}): the records filed {errors:?}");
        assert_eq!(got, solved_items[i], "item {i} ({key:x}): the records answer differently");
    }
    let opened = records.opened();
    assert_eq!(lang_answers(records, &front.index), solved_langs, "the lang index over records (lang crates {lang_crates:?})");
    let mut lazy = Buckets::lazy(records);
    let (impls, _) = impl_answers(records, front.index.lang("sized"), frame, root, &mut lazy);
    assert_eq!(impls, solved_impls, "the impl index over records");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    println!("RECORDS: {} items, every answer equal over both sources", chosen.len());
    println!(
        "RECORDS: first item {:?}, tenth item {:?}; names records opened {} ({} bytes), scopes decoded {} ({} bytes); items records opened {} ({} bytes), member lists {}, texts read {} ({} bytes)",
        times[0], times[9], opened.names.crates, opened.names.bytes, opened.names.scopes, opened.names.scope_bytes, opened.items.crates, opened.items.bytes, opened.items.member_lists, opened.items.texts, opened.items.text_bytes
    );

    // The planted bug: core's impl rows withheld. `impl Clone for bool` is core's, so the
    // candidates of `bool: Clone` must be fewer than the solved names', and the answers differ.
    let (planted, planted_frame) = reader(keys, crate_names, kept_planted);
    planted.plant_no_impls(core);
    let planted_front = Front2::from_records(planted, planted_frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over planted records: {e}"));
    let mut planted_lazy = Buckets::lazy(planted);
    let (planted_impls, planted_candidates) = impl_answers(planted, planted_front.index.lang("sized"), planted_frame, root, &mut planted_lazy);
    assert_ne!(planted_impls, solved_impls, "the planted bug (core's impl rows withheld) was not caught");
    assert!(planted_candidates < solved_candidates, "core's impl rows withheld: {planted_candidates} candidates, {solved_candidates} solved");
    println!("PLANT caught: core's impl rows withheld: bool: Clone has {planted_candidates} candidates over records, {solved_candidates} solved");

    // No record kept for core: a typed error naming the crate, never an empty answer taken
    // for a fact.
    let (bare, _) = reader(keys, crate_names, kept_no_core);
    let sized = front.index.lang("sized").expect("core's `Sized`");
    assert!(bare.text_of(sized).is_none(), "an item of a crate with no items record has no text");
    let filed: Vec<String> = bare.errors().iter().map(|e| e.to_string()).collect();
    let wanted = format!("no items record for crate `core` (crate {core})");
    assert!(filed.contains(&wanted), "wanted `{wanted}` among {filed:?}");
    println!("NO RECORD: {wanted}");
    println!("NOT YET: no body is checked over records (Store, Builder, World, check loop take &Names)");
}
