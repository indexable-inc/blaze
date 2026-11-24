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
//! THE PROOF (a body CHECKED over records): the same eleven bodies are lowered and checked
//! (`body::lower_fn`, then `check::check_body`) over the solved names, one store for the
//! eleven, each outcome kept as text; the solved names are dropped; then a FRESH reader (so
//! what it opened is the bodies' alone), a fresh `Store`, `Keys`, `Prepares` and `Consts`, the
//! same calls through the same function (`body_checked`), no error in the reader's log, and
//! equal text. The text of a finished check is the `Debug` form of `blaze_typeck::Checked`
//! (every table of the record: types, adjustments, callees, diagnostics, reads, needs); of a
//! failure, `CheckFail`'s `Debug` form. ONE thing is compared by identity and not as printed:
//! a gap about a candidate impl whose header was refused or unread (`Gap::RefusedImpl`,
//! `Gap::UnreadImpl`) prints WHERE the impl is, and that place is `file:line` over the solved
//! names and the module's path over records (no text is opened to count lines:
//! `BodyWorld::place_shown`). Such a gap is compared as the impl's key, the question asked
//! and the reason; its printed place is left out (`gap_identity`).
//!
//! PLANTED BUG of the proof: a third fresh reader with ONE item row withheld
//! (`Records::plant_no_item`: the row of `Out`, which `main`'s signature names) must give
//! `main` a different outcome, and the test checks it does.
//!
//! One `bool: Clone` question over a reader that was asked nothing else says how many source
//! texts a candidates question opens. `Buckets::admit` files an impl from its ROW where the
//! row holds the trait's path and the self type's head, so no text is read to file it; the
//! text is still read for an impl the row does not file (a self type that is no path,
//! reference, pointer, slice, array, tuple or `!`: `dyn Tr`, `fn(A)`, `<T as Tr>::Out`; a
//! path with `$crate` or a raw identifier; a trait path that names no trait as written) and
//! for a type alias an impl is written on. The same question is asked of another fresh
//! reader with the rows off (`Buckets::plant_no_rows`: every impl's header and item text is
//! read, as before the rows): THE CAUSE REMOVED. The test checks that both answer what the
//! solved names answer and that the rows open fewer texts. Expected (said before any run):
//! with the rows off, every file and expansion of core that holds an impl, more than 1,000
//! texts; from rows, fewer than 100. THE FIRST RUN SAID OTHERWISE for the rows (tree
//! 60d608607a): rows off 12,260 texts; from rows 7,222 texts, 19,806 impls filed from rows
//! and 8,913 from header text, over 70 crates opened. Two causes, neither a guess of this
//! file: (1) the second question asked (`impl bool`, `Home::Incoherent`) asks EVERY crate's
//! items record whether it may write incoherent impls and admits each that may, so the
//! number is not core's alone; (2) 8,913 rows do not file their impl: the line `row misses`
//! prints them by class (`buckets::ROW_MISSES`), which names what the row must hold next.
//!
//! It should print (numbers are the run's), and writes the lines from `front2 records:` on to
//! the file `FRONT2_RECORDS_OUT` names, when set:
//! `RECORDS: 11 items, every answer equal over both sources`
//! `RECORDS: first item <t>, tenth item <t>; names records opened <n> (<b> bytes); items records opened <n> (<b> bytes), texts read <n> (<b> bytes)`
//! `PLANT caught: core's impl rows withheld: bool: Clone has <n> candidates over records, <m> solved`
//! `NO RECORD: no items record for crate `core` (crate <k>)`
//! `front2 records: bodies 11 equal`
//! `crates opened <n>`
//! `record bytes <n>`
//! `texts opened <n> (bytes <n>)`
//! `first body check over records <n> us`
//! `tenth body check over records <n> us`
//! `planted: item row withheld -> differs`
//! `bool: Clone alone over a fresh reader: texts opened <n> (crates <n>); impls filed from rows <n>, from header text <n>`
//! `bool: Clone, then impl bool, over that reader: texts opened <n> (crates <n>); impls filed from rows <n>, from header text <n>`
//! `bool: Clone with the rows off: texts opened <n> (crates <n>); impls filed from header text <n>`
//! `row misses: header unread <n>, trait unread <n>, self type other <n>, self path not plain <n>, trait path not plain <n>, trait not resolved <n>`
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs.

#![feature(allocator_api)]

use std::collections::HashMap;
use std::time::Instant;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::buckets::{Ask, Buckets, Home, UNFILED_LIST, inherent_home, prepare_home, trait_home};
use blaze_ladder::front2::check::{self, BuildOrder, CheckFail, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::{Keys, checker_of};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Opened, Records};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::world_items::Gap;
use blaze_ladder::front2::{Front2, fill, langs};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Kind, Ns, Paths};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_typeck::Shape;

/// His `Cargo.toml`'s edition line.
const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs`, as written.
const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
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
    /// Each expansion's origin column ([`made_origins`]): a macro-written item's words resolve by
    /// their marks over records as over the solved names.
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
    /// The crate whose items record is not kept (the third reader's).
    no_items: Option<usize>,
}

/// The origin column an expansion's text keeps, as a loader serves it: empty for a text that
/// keeps none by design (blaze-scope `OriginsError::Rewritten`, `Unplaced`). Any other error is
/// the solve's, and the test stops on it.
fn made_origins(call: ItemKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
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

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
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
    out.push_str(&format!("item_start {:?}\n", names.item_start(item)));
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
        out.push_str(&format!("macro {name}: {:?}\n", paths.macro_body(module, false, &[name], blaze_own::Alloc::global()).map(|b| b.is_some())));
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
    let mut store = Store::new(sized, frame, blaze_own::Alloc::global());
    let clone = Paths { names }.path(root, false, &["Clone"], Ns::Type).unwrap_or_else(|e| panic!("`Clone` from the program's root: {e}"));
    assert_eq!(clone.kind, Kind::Trait, "`Clone` names a trait");
    keys.item(clone.key).unwrap_or_else(|e| panic!("filing `Clone`: {e}"));
    let trait_ = checker_of(clone.key);
    // As production does before reading a home: the question's coherence wants are made first.
    prepare_home(names, &mut keys, &mut store, Ask::Trait(trait_, Shape::Bool)).unwrap_or_else(|e| panic!("prepare coherence: {e}"));
    let home = trait_home(names, &keys, &store, trait_, Shape::Bool).unwrap_or_else(|e| panic!("trait home: {e}"));
    b.admit(names, home);
    b.admit_asked(names, &keys, home, Ask::Trait(trait_, Shape::Bool)).unwrap_or_else(|e| panic!("admit `bool: Clone`: {e}"));
    let of_trait = b.trait_candidates(trait_, Shape::Bool, home);
    let inherent = inherent_home(names, &keys, &store, Shape::Bool).unwrap_or_else(|e| panic!("inherent home: {e}"));
    b.admit(names, inherent);
    b.admit_asked(names, &keys, inherent, Ask::Inherent(Shape::Bool)).unwrap_or_else(|e| panic!("admit `impl bool`: {e}"));
    let of_bool = b.inherent_candidates(Shape::Bool, inherent);
    (format!("bool: Clone in {home:?}: {of_trait:?}\nimpl bool in {inherent:?}: {of_bool:?}\n"), of_trait.len())
}

/// What identifies a gap: for a candidate impl whose header was refused or unread, the impl's
/// key, the question asked and the reason, WITHOUT the printed place (`file:line` over the
/// solved names, the module's path over records: `BodyWorld::place_shown`; the place is
/// followed by ` [asked: <question>]`, which is kept). Any other gap is its `Debug` text.
fn gap_identity(gap: &Gap) -> String {
    // The text from ` [asked: ` on; a text without it is kept whole (and then compared whole).
    let asked = |text: &str| text.find(" [asked: ").map_or(text.to_string(), |at| text[at..].to_string());
    match gap {
        Gap::RefusedImpl { impl_, place, why } => format!("RefusedImpl {impl_:x}{}: {why}", asked(place)),
        Gap::UnreadImpl(impl_, text) => format!("UnreadImpl {impl_:x}{}", asked(text)),
        other => format!("{other:?}"),
    }
}

/// A check's outcome as text, for comparing the two sources: a finished check's whole record
/// (`Checked`'s `Debug` form), a failure's `Debug` form, a gap list by each gap's identity
/// (sorted: the loop sorts gaps by their `Debug` text, which holds the printed place).
fn outcome(checked: &Result<blaze_typeck::Checked<'static>, CheckFail>) -> String {
    match checked {
        Ok(done) => format!("checked: {done:?}"),
        Err(CheckFail::Gap { owner, gaps }) => {
            let mut lines: Vec<String> = gaps.iter().map(gap_identity).collect();
            lines.sort();
            format!("gaps of {owner:?}: {lines:#?}")
        }
        Err(e) => format!("not checked: {e:?}"),
    }
}

/// The fn `item` lowered and checked over `names`, as text. ONE function over both sources:
/// what differs between two calls is the source alone (and the records the caller keeps for
/// the eleven bodies: the key map, the head store, the impl index, the constants).
fn body_checked<'n, S: NamesSource<'n>>(
    names: S,
    prepared: &'n Prepares<'n>,
    keys: &mut Keys,
    index: &Index<'n>,
    store: &mut Store<'n>,
    buckets: &mut Option<Buckets>,
    consts: &mut Consts<'n>,
    item: ItemKey,
) -> String {
    body_checked_timed(names, prepared, keys, index, store, buckets, consts, item).0
}

/// [`body_checked`] with its phases' wall times: lowering, spelling and scope, the check.
#[allow(clippy::too_many_arguments)]
fn body_checked_timed<'n, S: NamesSource<'n>>(
    names: S,
    prepared: &'n Prepares<'n>,
    keys: &mut Keys,
    index: &Index<'n>,
    store: &mut Store<'n>,
    buckets: &mut Option<Buckets>,
    consts: &mut Consts<'n>,
    item: ItemKey,
) -> (String, [std::time::Duration; 3]) {
    let mut times = [std::time::Duration::ZERO; 3];
    let start = Instant::now();
    let body = match lower_fn(names, prepared, keys, store, item) {
        Ok(body) => body,
        Err(e) => return (format!("not lowered: {e}"), times),
    };
    times[0] = start.elapsed();
    let start = Instant::now();
    let view = body.lowered.view();
    let spell = match Spelling::of(&view) {
        Ok(spell) => spell,
        Err(e) => return (format!("not spelled: {e:?}"), times),
    };
    let scope = body.scope(&spell);
    times[1] = start.elapsed();
    let start = Instant::now();
    let mut records = check::Records { names, keys, index, store, buckets, order: BuildOrder::AsAsked, consts: Some((consts, prepared)) };
    let checked = check_body(&mut records, &body, &scope);
    let got = format!("{}\nreads filed: {:?}", outcome(&checked), records.store.reads.get(body.owner));
    times[2] = start.elapsed();
    (got, times)
}

/// The terms under `roots` in `t` as text, in preorder, each node by its own content (never by
/// its index: two stores number the same term differently). A work list; a diagnosis aid only.
fn terms(t: blaze_typeck::PTys<'_>, roots: &[blaze_typeck::PTy]) -> String {
    use blaze_typeck::PNode;
    let mut out = String::new();
    let mut work: Vec<blaze_typeck::PTy> = roots.iter().rev().copied().collect();
    while let Some(p) = work.pop() {
        let node = match t.node(p) {
            Ok(node) => node,
            Err(e) => {
                out.push_str(&format!(" <no node: {e:?}>"));
                continue;
            }
        };
        let list = |l| t.list(l).map(|s| s.to_vec()).unwrap_or_default();
        let (shown, kids): (String, Vec<blaze_typeck::PTy>) = match node {
            PNode::Tuple(l) => ("Tuple".to_string(), list(l)),
            PNode::Record(s, l) => (format!("Record({s:?})"), list(l)),
            PNode::Union(l) => ("Union".to_string(), list(l)),
            PNode::Ref(m, x) => (format!("Ref({m:?})"), vec![x]),
            PNode::Ptr(m, x) => (format!("Ptr({m:?})"), vec![x]),
            PNode::Array(a, b) => ("Array".to_string(), vec![a, b]),
            PNode::Slice(a) => ("Slice".to_string(), vec![a]),
            PNode::Pat(a, l) => ("Pat".to_string(), [vec![a], list(l)].concat()),
            PNode::Adt(k, l) => (format!("Adt({k:?})"), list(l)),
            PNode::FnDef(k, l) => (format!("FnDef({k:?})"), list(l)),
            PNode::FnPtr(l, h) => (format!("FnPtr({h:?})"), list(l)),
            PNode::Closure { owner, ordinal, args } => (format!("Closure({owner:?}, {ordinal})"), list(args)),
            PNode::Future(e, a, b) => (format!("Future({e:?})"), vec![a, b]),
            PNode::Alias { trait_, name, args } => (format!("Alias({trait_:?}, {name:?})"), list(args)),
            PNode::Opaque(k, l) => (format!("Opaque({k:?})"), list(l)),
            PNode::ConstValue { ty, bits } => (format!("ConstValue({bits})"), vec![ty]),
            PNode::Dynamic(l) => ("Dynamic".to_string(), list(l)),
            PNode::ExTrait(k, l) => (format!("ExTrait({k:?})"), list(l)),
            PNode::ExProj { trait_, name, args, term } => (format!("ExProj({trait_:?}, {name:?})"), [list(args), vec![term]].concat()),
            other => (format!("{other:?}"), Vec::new()),
        };
        out.push_str(&format!(" {shown}/{}", kids.len()));
        work.extend(kids.into_iter().rev());
    }
    out
}

/// Every head `store` holds, as text by the question that reads it (`FnSig <key>`, `Adt <key>`,
/// `ImplHeader <key>`, `Predicates <key>`): the parts the checker's read-list digest covers,
/// its terms by content ([`terms`]), and the item's text as `names` holds it. For telling
/// which head two sources made differently (`FRONT2_RECORDS_DIFF`).
fn heads_dump<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>) -> std::collections::BTreeMap<String, String> {
    use blaze_typeck::PClause;
    let t = store.tys.view();
    let text = |k: blaze_typeck::ItemKey| -> String {
        let lib = keys.library_in(names, k);
        let text = lib.and_then(|lib| names.text_of(lib));
        let shown: String = text.map(|t| t.impl_header.unwrap_or(t.text).chars().take(700).collect()).unwrap_or_default();
        format!("names key {:?} place {:?} text `{shown}`", lib.map(|l| format!("{l:x}")), lib.and_then(|l| names.place_of(l)).map(|p| (p.krate, p.at, p.item)))
    };
    let mut out = std::collections::BTreeMap::new();
    for (k, f) in store.fns.iter() {
        let shown: String = f.text.chars().take(700).collect();
        out.insert(
            format!("FnSig {k:?}"),
            format!(
                "parent {} own {} declared {} self_param {} header {:?}\n  inputs:{}\n  output:{}\n  fn text `{shown}`\n  {}",
                f.parent,
                f.own,
                f.declared,
                f.self_param,
                f.header,
                terms(t, &f.inputs),
                terms(t, &[f.output]),
                text(*k)
            ),
        );
    }
    // The same heads by their handles' numbers in the store's table (`RAW <question>`): what
    // two stores that interned their terms in different orders hold differently.
    for (k, f) in store.fns.iter() {
        out.insert(format!("RAW FnSig {k:?}"), format!("inputs {:?} output {:?}", f.inputs, f.output));
    }
    for (k, a) in store.adts.iter() {
        out.insert(format!("RAW Adt {k:?}"), format!("fields {:?}", a.fields));
    }
    for (k, i) in store.impls.iter() {
        out.insert(format!("RAW ImplHeader {k:?}"), format!("self {:?} trait args {:?} assoc {:?}", i.self_ty, i.trait_args, i.assoc));
    }
    for (k, p) in store.preds.iter() {
        out.insert(format!("RAW Predicates {k:?}"), format!("{:?}", p.clauses));
    }
    for (k, c) in store.ctors.iter() {
        out.insert(format!("FnSig {k:?}"), format!("ctor of {:?} generics {}\n  inputs:{}\n  output:{}", c.adt, c.generics, terms(t, &c.inputs), terms(t, &[c.output])));
    }
    for (k, a) in store.adts.iter() {
        let fields: Vec<String> = a.fields.iter().map(|f| format!("{:?}:{}", f.name, terms(t, &[f.ty]))).collect();
        out.insert(format!("Adt {k:?}"), format!("kind {:?} generics {}\n  variants {:?}\n  fields {}\n  {}", a.kind, a.generics, a.variants, fields.join("\n         "), text(*k)));
    }
    for (k, i) in store.impls.iter() {
        let args = t.list(i.trait_args.list()).map(|s| s.to_vec()).unwrap_or_default();
        let assoc: Vec<String> = i.assoc.iter().map(|(n, ty)| format!("{n:?}:{}", terms(t, &[*ty]))).collect();
        out.insert(
            format!("ImplHeader {k:?}"),
            format!(
                "generics {} polarity {:?} trait {:?}\n  self:{}\n  trait args:{}\n  assoc {}\n  {}",
                i.generics,
                i.polarity,
                i.trait_,
                terms(t, &[i.self_ty]),
                terms(t, &args),
                assoc.join("\n        "),
                text(*k)
            ),
        );
    }
    for (k, p) in store.preds.iter() {
        let clauses: Vec<String> = p
            .clauses
            .iter()
            .map(|c| match c {
                PClause::Trait { trait_, args } => format!("Trait {trait_:?}:{}", terms(t, &t.list(*args).map(|s| s.to_vec()).unwrap_or_default())),
                PClause::Projection { trait_, args, name, term } => {
                    format!("Projection {trait_:?} {name:?}:{} =={}", terms(t, &t.list(*args).map(|s| s.to_vec()).unwrap_or_default()), terms(t, &[*term]))
                }
                PClause::Const { cond, args } => format!("Const {cond:?}:{}", terms(t, &t.list(*args).map(|s| s.to_vec()).unwrap_or_default())),
                PClause::TypeOutlives { ty, region } => format!("TypeOutlives {region:?}:{}", terms(t, &[*ty])),
                PClause::RegionOutlives { long, short } => format!("RegionOutlives {long:?}: {short:?}"),
                PClause::ForFields { container, trait_, args } => {
                    format!("ForFields {trait_:?} {}:{}", terms(t, &[*container]), terms(t, t.list(*args).unwrap_or(&[])))
                }
            })
            .collect();
        out.insert(format!("Predicates {k:?}"), format!("conds {}\n  {}\n  {}", p.conds.len(), clauses.join("\n  "), text(*k)));
    }
    out
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
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new(), no_items };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            // The bucket index resolved by the whole graph's names (`encode_with`): every impl
            // header's paths are walked to their end, as the reader's names would walk them.
            kept.items.push(aligned(&items_record::encode_with(c, names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            // By each file's path in its package: what the items record asks for (D96).
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
            kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
        }
        kept
    };
    let (kept, kept_planted, kept_no_core, chosen, root, solved_items, solved_langs, solved_impls, solved_candidates, kept_bodies, kept_bodies_planted, kept_question, kept_question_text, solved_bodies, (io, out_ty), solved_heads) = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
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
        // The proof's solved half: the eleven bodies lowered and checked over the solved names,
        // one store, key map, impl index and set of constants for the eleven.
        // D104: `Io` is in no prelude; it is bstd's world type at `std::Io`.
        let io = Paths { names: &names }.path(root, false, &["std", "Io"], Ns::Type).unwrap_or_else(|e| panic!("`std::Io` from his crate root: {e}")).key;
        // `Out`: the type `main`'s signature names (D104), for the planted bug.
        let out_ty = Paths { names: &names }.path(root, false, &["Out"], Ns::Type).unwrap_or_else(|e| panic!("`Out` from his crate root: {e}")).key;
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let (mut store, mut body_buckets, mut consts) = (Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global()), None, Consts::default());
        let mut solved_bodies: Vec<String> = Vec::new();
        for key in chosen.iter() {
            solved_bodies.push(body_checked(&names, prepared, &mut front.keys, &front.index, &mut store, &mut body_buckets, &mut consts, *key));
        }
        (
            make(&names, None),
            make(&names, None),
            make(&names, Some(core)),
            chosen,
            root,
            solved_items,
            solved_langs,
            solved_impls,
            solved_candidates,
            make(&names, None),
            make(&names, None),
            make(&names, None),
            make(&names, None),
            solved_bodies,
            (io, out_ty),
            // The solved heads as text, only when a diff file is asked for.
            std::env::var_os("FRONT2_RECORDS_DIFF").map(|_| heads_dump(&names, &front.keys, &store)),
        )
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
        "RECORDS: first item {:?}, tenth item {:?}; names records opened {} ({} bytes); items records opened {} ({} bytes), texts read {} ({} bytes)",
        times[0], times[9], opened.names.crates, opened.names.bytes, opened.items.crates, opened.items.bytes, opened.items.texts, opened.items.text_bytes
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

    // THE PROOF: the eleven bodies checked over records alone. A fresh reader, so what it
    // opened is what the lang rows and these bodies asked for and nothing else.
    let (body_records, body_frame) = reader(keys, crate_names, kept_bodies);
    let mut body_front = Front2::from_records(body_records, body_frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over the bodies' records: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut store, mut body_buckets, mut consts) = (Store::new(body_front.index.lang("sized"), body_frame, blaze_own::Alloc::global()), None, Consts::default());
    let mut body_times = Vec::new();
    // Per body: its phases and what it opened beyond the bodies before it (the first body's
    // line says where its time goes).
    let mut body_phases: Vec<([std::time::Duration; 3], Opened, Opened)> = Vec::new();
    for (i, key) in chosen.iter().enumerate() {
        let before = body_records.opened();
        let start = Instant::now();
        let (got, phases) = body_checked_timed(body_records, prepared, &mut body_front.keys, &body_front.index, &mut store, &mut body_buckets, &mut consts, *key);
        body_times.push(start.elapsed());
        body_phases.push((phases, before, body_records.opened()));
        let errors: Vec<String> = body_records.errors().iter().map(|e| e.to_string()).collect();
        // The check scripts cut a panic's text: `FRONT2_RECORDS_DIFF` names a file for the whole
        // of what a failing body filed and of both outcomes.
        if let Some(path) = std::env::var_os("FRONT2_RECORDS_DIFF")
            && (!errors.is_empty() || got != solved_bodies[i])
        {
            // The heads the two stores hold differently, both texts: a read's digest that
            // differs is one of these.
            let mut heads = String::new();
            if let Some(solved) = &solved_heads {
                let over_records = heads_dump(body_records, &body_front.keys, &store);
                // The digest the read list of `outcome` holds for `question` (`Kind ItemKey(n)`).
                let digest_in = |outcome: &str, question: &str| -> Option<String> {
                    let (kind, key) = question.split_once(' ')?;
                    let row = format!("({kind}({key}), ");
                    let at = outcome.find(&row)? + row.len();
                    Some(outcome[at..].chars().take_while(|c| c.is_ascii_digit()).collect())
                };
                for (question, text) in over_records.iter().filter(|(q, _)| !q.starts_with("RAW ")) {
                    let Some(s) = solved.get(question) else { continue };
                    if s != text {
                        heads.push_str(&format!("HEAD {question}: THE CONTENT DIFFERS\n OVER RECORDS: {text}\n SOLVED: {s}\n"));
                    }
                    let (a, b) = (digest_in(&got, question), digest_in(&solved_bodies[i], question));
                    if a != b {
                        let raw = format!("RAW {question}");
                        heads.push_str(&format!(
                            "READ {question}: digest over records {a:?}, solved {b:?}; content equal: {}\n CONTENT: {text}\n HANDLES OVER RECORDS: {:?}\n HANDLES SOLVED: {:?}\n",
                            s == text,
                            over_records.get(&raw),
                            solved.get(&raw)
                        ));
                    }
                }
                heads.push_str(&format!("HEADS: {} over records, {} solved\n", over_records.len(), solved.len()));
            }
            let whole = format!("body {i} ({key:x})\nFILED BY THE RECORDS:\n{}\n{heads}OVER RECORDS:\n{got}\nOVER THE SOLVED NAMES:\n{}\n", errors.join("\n"), solved_bodies[i]);
            std::fs::write(&path, whole).expect("writing FRONT2_RECORDS_DIFF");
        }
        assert!(errors.is_empty(), "body {i} ({key:x}): the records filed {errors:?}");
        // Compared by a bool so a failure prints where the texts part, not two whole records.
        let parts = got.bytes().zip(solved_bodies[i].bytes()).position(|(a, b)| a != b).unwrap_or(got.len().min(solved_bodies[i].len()));
        let from = (0..=parts.saturating_sub(60)).rev().find(|at| got.is_char_boundary(*at) && solved_bodies[i].is_char_boundary(*at)).unwrap_or(0);
        let shown = |text: &str| text[from..].chars().take(160).collect::<String>();
        assert!(
            got == solved_bodies[i],
            "body {i} ({key:x}): checked over records it differs from the solved names at byte {parts}: records `{}` solved `{}`",
            shown(&got),
            shown(&solved_bodies[i])
        );
    }
    let body_opened = body_records.opened();

    // The proof's planted bug: the item row of `Out` withheld. `main`'s signature names `Out`
    // (D104: `out: Out`), so its head cannot be made and `main`'s outcome must differ.
    let (withheld, withheld_frame) = reader(keys, crate_names, kept_bodies_planted);
    withheld.plant_no_item(out_ty);
    let mut withheld_front = Front2::from_records(withheld, withheld_frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over the withheld records: {e}"));
    let withheld_prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut withheld_store, mut withheld_buckets, mut withheld_consts) = (Store::new(withheld_front.index.lang("sized"), withheld_frame, blaze_own::Alloc::global()), None, Consts::default());
    let withheld_main = body_checked(withheld, withheld_prepared, &mut withheld_front.keys, &withheld_front.index, &mut withheld_store, &mut withheld_buckets, &mut withheld_consts, chosen[0]);
    assert_ne!(withheld_main, solved_bodies[0], "the planted bug (the item row of `Out` withheld) was not caught: `main` checks the same");

    // One candidates question over a reader asked nothing else: the texts it opens.
    let (asked, asked_frame) = reader(keys, crate_names, kept_question);
    let asked_front = Front2::from_records(asked, asked_frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over the question's records: {e}"));
    let mut asked_lazy = Buckets::lazy(asked);
    // The trait question ALONE first (`bool: Clone`: its home is core): what it opens and
    // files, before `impl_answers` asks it again (nothing more is opened for a crate filed)
    // and then asks the inherent question, whose home is every crate that may write an
    // incoherent impl.
    let before_alone = asked.opened();
    let (alone_opened, alone_rows, alone_text, alone_index, alone_asked, alone_reads, alone_home, clone_lib, alone_candidates) = {
        let mut alone_keys = Keys::new();
        let mut alone_store = Store::new(asked_front.index.lang("sized"), asked_frame, blaze_own::Alloc::global());
        let clone = Paths { names: asked }.path(root, false, &["Clone"], Ns::Type).unwrap_or_else(|e| panic!("`Clone` from the program's root over records: {e}"));
        alone_keys.item(clone.key).unwrap_or_else(|e| panic!("filing `Clone`: {e}"));
        // As production does before reading a home: the question's coherence wants are made first.
        prepare_home(asked, &mut alone_keys, &mut alone_store, Ask::Trait(checker_of(clone.key), Shape::Bool)).unwrap_or_else(|e| panic!("prepare coherence: {e}"));
        let home = trait_home(asked, &alone_keys, &alone_store, checker_of(clone.key), Shape::Bool).unwrap_or_else(|e| panic!("trait home: {e}"));
        asked_lazy.admit(asked, home);
        asked_lazy.admit_asked(asked, &alone_keys, home, Ask::Trait(checker_of(clone.key), Shape::Bool)).unwrap_or_else(|e| panic!("admit `bool: Clone`: {e}"));
        let candidates = asked_lazy.trait_candidates(checker_of(clone.key), Shape::Bool, home);
        (
            asked.opened(),
            asked_lazy.index().filed_from_rows,
            asked_lazy.index().filed_from_text,
            asked_lazy.filed_from_index(),
            asked_lazy.crates_asked(),
            asked_lazy.trait_reads(checker_of(clone.key), Shape::Bool).to_vec(),
            home,
            clone.key,
            candidates,
        )
    };
    // `Clone` has no parameter but `Self` and `bool` is no crate's local type: the orphan rules
    // leave the trait's own crate (module docs of this section's asserts, below).
    assert_eq!(alone_home.all, Home::In(core, None), "the home of `bool: Clone` is core alone");
    assert_eq!(alone_asked, 1, "`bool: Clone` asked {alone_asked} crates by bucket; its home is core alone");
    assert!(alone_index > 0, "`impl Clone for bool` was not filed from core's bucket index");
    let (asked_impls, _) = impl_answers(asked, asked_front.index.lang("sized"), asked_frame, root, &mut asked_lazy);
    assert_eq!(asked_impls, solved_impls, "the impl index over a fresh reader");
    let errors: Vec<String> = asked.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the question's records filed {errors:?}");
    let asked_opened = asked.opened();
    // The cause removed (rule 23): the same question over another fresh reader, every impl
    // filed from its header's text as before the rows were read. It must answer the same and
    // open more texts.
    let (by_text, by_text_frame) = reader(keys, crate_names, kept_question_text);
    let by_text_front = Front2::from_records(by_text, by_text_frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over the text question's records: {e}"));
    let mut by_text_lazy = Buckets::lazy(by_text);
    by_text_lazy.plant_no_rows();
    let (by_text_impls, _) = impl_answers(by_text, by_text_front.index.lang("sized"), by_text_frame, root, &mut by_text_lazy);
    assert_eq!(by_text_impls, solved_impls, "the impl index over a fresh reader, every impl filed from text");
    let by_text_opened = by_text.opened();
    assert_eq!(by_text_lazy.index().filed_from_rows, 0,"with the rows off no impl is filed from a row");
    assert!(asked_lazy.filed_from_index() > 0, "no impl was filed from a bucket of an items record");
    assert_eq!(by_text_lazy.filed_from_index(), 0, "with the rows off no impl is filed from a bucket");

    // PLANTED BUG for the bucket index: the bucket row of one candidate of `bool: Clone` (an
    // impl core's index files under (`Clone`, `bool`)) is withheld; over a new index on the
    // same reader the question's candidates must lose exactly that impl.
    let bool_key = blaze_scope::builtin_type("bool").expect("`bool` is a builtin type").key;
    let clone_bool = blaze_scope::items_record::buckets::BucketKey::of_trait(clone_lib, blaze_scope::items_record::buckets::Head::Named(bool_key));
    let mut in_bucket: Vec<ItemKey> = Vec::new();
    asked.bucket(core, &clone_bool, &mut |it| in_bucket.push(it.key)).unwrap_or_else(|| panic!("core's record answers no bucket"));
    assert!(!in_bucket.is_empty() && in_bucket.iter().all(|k| alone_candidates.contains(k)), "core files {in_bucket:?} under (`Clone`, `bool`); the question's candidates are {alone_candidates:?}");
    asked.plant_no_bucket_impl(in_bucket[0]);
    let planted_candidates = {
        let mut planted_index = Buckets::lazy(asked);
        let planted_keys = Keys::new();
        planted_keys.item(clone_lib).unwrap_or_else(|e| panic!("filing `Clone`: {e}"));
        planted_index.admit_asked(asked, &planted_keys, alone_home, Ask::Trait(checker_of(clone_lib), Shape::Bool)).unwrap_or_else(|e| panic!("admit `bool: Clone` (planted): {e}"));
        planted_index.trait_candidates(checker_of(clone_lib), Shape::Bool, alone_home)
    };
    let expected: Vec<ItemKey> = alone_candidates.iter().copied().filter(|k| *k != in_bucket[0]).collect();
    assert_eq!(planted_candidates.to_vec(), expected, "the planted bug (one bucket row withheld) was not caught: `bool: Clone` has the same candidates");
    assert!(
        asked_opened.items.texts < by_text_opened.items.texts,
        "filing from rows opened {} texts, filing from text {}: the rows saved no text",
        asked_opened.items.texts,
        by_text_opened.items.texts
    );

    let lines = [
        format!("front2 records: bodies {} equal", chosen.len()),
        format!("crates opened {}", body_opened.items.crates),
        format!("record bytes {}", body_opened.items.bytes + body_opened.names.bytes),
        format!("texts opened {} (bytes {})", body_opened.items.texts, body_opened.items.text_bytes),
        format!("first body check over records {} us", body_times[0].as_micros()),
        format!("tenth body check over records {} us", body_times[9].as_micros()),
        {
            // Where the first body's time goes, and what it opened beyond the reader's start.
            let ([lower, spell, check], b, a) = body_phases[0];
            format!(
                "first body phases: lower {} us, spell+scope {} us, check {} us; opened: names records {} ({} bytes), items records {} ({} bytes), texts {} ({} bytes)",
                lower.as_micros(),
                spell.as_micros(),
                check.as_micros(),
                a.names.crates - b.names.crates,
                a.names.bytes - b.names.bytes,
                a.items.crates - b.items.crates,
                a.items.bytes - b.items.bytes,
                a.items.texts - b.items.texts,
                a.items.text_bytes - b.items.text_bytes
            )
        },
        "planted: item row withheld -> differs".to_string(),
        format!(
            "bool: Clone alone over a fresh reader: texts opened {} (crates {}); impls filed from rows {alone_rows}, from header text {alone_text}",
            alone_opened.items.texts, alone_opened.items.crates
        ),
        format!(
            "bool: Clone, then impl bool, over that reader: texts opened {} (crates {}); impls filed from rows {}, from header text {}",
            asked_opened.items.texts, asked_opened.items.crates, asked_lazy.index().filed_from_rows, asked_lazy.index().filed_from_text
        ),
        format!(
            "bool: Clone with the rows off: texts opened {} (crates {}); impls filed from header text {}",
            by_text_opened.items.texts, by_text_opened.items.crates, by_text_lazy.index().filed_from_text
        ),
        format!(
            "row misses: {}",
            blaze_ladder::front2::buckets::ROW_MISSES.iter().zip(asked_lazy.index().row_misses.iter()).map(|(class, n)| format!("{class} {n}")).collect::<Vec<_>>().join(", ")
        ),
        format!(
            "bool: Clone alone, by bucket: crates asked {alone_asked}; items records opened by the question {}; impls filed from the index {alone_index}; (crate, bucket) facts read {}",
            alone_opened.items.crates - before_alone.items.crates,
            alone_reads.len()
        ),
        format!(
            "bool: Clone, then impl bool, by bucket: crates asked {} ({}); impls filed from the index {}",
            asked_lazy.crates_asked(),
            asked_lazy.crates_asked_sorted().iter().map(|c| format!("`{}`", crate_names[*c])).collect::<Vec<_>>().join(", "),
            asked_lazy.filed_from_index()
        ),
        format!("not filed when written, in the crates asked: {}", asked_lazy.not_filed().iter().map(|(class, n)| format!("{class} {n}")).collect::<Vec<_>>().join(", ")),
        "planted: bucket row withheld -> candidates differ".to_string(),
    ];
    for line in lines.iter() {
        println!("{line}");
    }
    // The check scripts keep no test output: `FRONT2_RECORDS_OUT` names a file for the lines.
    let out = std::env::var_os("FRONT2_RECORDS_OUT");
    if let Some(path) = out.as_ref() {
        std::fs::write(path, lines.join("\n") + "\n").expect("writing FRONT2_RECORDS_OUT");
    }

    // THE ARCHITECT'S TEST (drive.md, "End state: the whole pipeline on the command", piece
    // 12): "`Display for a::T` opens `core` and `a`". Here `a::T` is `Io` (a nominal type of
    // blaze's own standard library, in the prelude the program's root sees) and the trait is core's
    // `fmt::Display`. The Rust Reference, "Implementations", "Trait implementation coherence",
    // "Orphan rules": "Given `impl<P1..=Pn> Trait<T1..=Tn> for T0`, an `impl` is valid only if
    // at least one of the following is true: `Trait` is a local trait; All of: At least one of
    // the types `T0..=Tn` must be a local type. Let `Ti` be the first such type. No uncovered
    // type parameters `P1..=Pn` may appear in `T0..Ti` (excluding `Ti`)". `Display` has no
    // type parameter but `Self`, so `T0` is the only type: an impl answering `Io: Display` is
    // written in the crate that defines `Display` (core) or in the crate that defines `Io`.
    // The question must ask those two crates' records and no other, and its read rows must
    // name those two crates only (each with its unfiled list and the buckets asked). It runs
    // last: the lines above are written whatever it finds.
    let a = asked.crate_of(io).unwrap_or_else(|| panic!("`Io` ({io:x}) is of no crate"));
    let core_root = asked.root_module(core).expect("core's root module");
    let display = Paths { names: asked }.path(core_root, false, &["fmt", "Display"], Ns::Type).unwrap_or_else(|e| panic!("`fmt::Display` from core's root over records: {e}"));
    assert_eq!(display.kind, Kind::Trait, "`fmt::Display` names a trait");
    let mut display_keys = Keys::new();
    display_keys.item(display.key).unwrap_or_else(|e| panic!("filing `Display`: {e}"));
    display_keys.item(io).unwrap_or_else(|e| panic!("filing `Io`: {e}"));
    let mut display_store = Store::new(asked_front.index.lang("sized"), asked_frame, blaze_own::Alloc::global());
    let (display_trait, io_shape) = (checker_of(display.key), Shape::Adt(checker_of(io)));
    // As production does before reading a home: the question's coherence wants are made first.
    prepare_home(asked, &mut display_keys, &mut display_store, Ask::Trait(display_trait, io_shape)).unwrap_or_else(|e| panic!("prepare coherence: {e}"));
    let display_home = trait_home(asked, &display_keys, &display_store, display_trait, io_shape).unwrap_or_else(|e| panic!("trait home: {e}"));
    let before_display = asked.opened();
    let mut display_index = Buckets::lazy(asked);
    display_index.admit_asked(asked, &display_keys, display_home, Ask::Trait(display_trait, io_shape)).unwrap_or_else(|e| panic!("admit `Display`: {e}"));
    let after_display = asked.opened();
    let reads = display_index.trait_reads(display_trait, io_shape);
    let first_16 = |key: CrateItemKey| u128::from_be_bytes(key.0.0[..16].try_into().expect("sixteen bytes"));
    let mut read_crates: Vec<u128> = reads.iter().map(|r| r.krate).collect();
    read_crates.dedup();
    let mut want_crates = vec![first_16(keys[core]), first_16(keys[a])];
    want_crates.sort();
    let display_line = format!(
        "Display for Io (`{}`): crates asked {} (`core` and `{}`); items records opened by the question {}; (crate, bucket) facts read {}",
        crate_names[a],
        display_index.crates_asked(),
        crate_names[a],
        after_display.items.crates - before_display.items.crates,
        reads.len()
    );
    println!("{display_line}");
    if let Some(path) = out.as_ref() {
        std::fs::write(path, lines.join("\n") + "\n" + &display_line + "\n").expect("writing FRONT2_RECORDS_OUT");
    }
    assert_ne!(a, core, "`Io` is not core's");
    assert_eq!(display_home.all, Home::In(core, Some(a)), "the home of `Io: Display` is core and `{}`", crate_names[a]);
    assert_eq!(display_index.crates_asked(), 2, "`Io: Display` asked {} crates by bucket; the orphan rules allow two", display_index.crates_asked());
    assert_eq!(read_crates, want_crates, "the read rows of `Io: Display` name other crates than core and `{}`", crate_names[a]);
    assert_eq!(reads.iter().filter(|r| r.bucket == UNFILED_LIST).count(), 2, "each crate asked is read with its unfiled list: {reads:?}");
    let errors: Vec<String> = asked.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "`Io: Display` over records filed {errors:?}");
}
