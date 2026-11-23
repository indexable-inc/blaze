//! The items record ([`crate::items_record`]) against a fresh solve, question for question
//! (per-crate-entry.md, "Runner twenty-one", part 1's table): a random small graph of crates
//! with structs, enums, traits, inherent and trait impls of every self-type head, lang items,
//! `macro_rules!` definitions, items and members written by macro calls, an inline module and
//! a second file; solved; each crate's items record written and read back on demand through
//! [`RecordItems`]; every question the front end asks of the solved crates' `Names` asked of
//! the records too: the answers must be equal. Then a file is changed under the record: the
//! reader must refuse it as stale. Planted: an impl row dropped, a member list cut by one (in
//! the encoder), a text's digest not checked (in the reader); each must be caught.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_surface::ImplFlags;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::items_record::plant::{PLANT, Plant};
use crate::items_record::{CurrentFile, ItemsRecord, ItemsRecordError, LoadItems, RecordItems, SelfHead, encode};
use crate::package_path::PackagePath;
use crate::resolve::{At, CrateInput, CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const CRATES: [&str; 4] = ["k0", "k1", "k2", "k3"];

/// What every drawn crate has: one struct, one trait with a member of each kind.
const ALWAYS: &str = "#![no_core]\npub struct Always;\npub trait Tr { fn f(&self); const K: u8; type Out; }\n";

/// The parts a drawn crate may have, each whole by itself beside [`ALWAYS`].
const PARTS: [&str; 12] = [
    "#[lang = \"sized\"]\npub trait Sized {}\n",
    "pub struct Tup(pub u8);\npub struct Rec { pub a: u8 }\n",
    "pub enum E { A, B(u8), C { x: u8 } }\n",
    "pub trait Gen<T> { fn g(&self) {} }\nimpl<T> Gen<T> for &mut Always {}\nimpl<T> Gen<u8> for T {}\n",
    "impl Always { pub fn new() -> Always { Always } pub const Z: u8 = 0; }\n",
    "impl Tr for Always { fn f(&self) {} const K: u8 = 1; type Out = u8; }\n",
    "impl<T> Tr for (T, u8) { fn f(&self) {} }\nimpl Tr for [u8; 4] {}\nimpl Tr for [u8] {}\nimpl Tr for *const Always {}\nunsafe impl Tr for () {}\n",
    "#[macro_export]\nmacro_rules! mk { ($n:ident) => { pub struct $n; impl $n { pub fn made() {} } impl $crate::Tr for $n { fn f(&self) {} } } }\nmk!(Made);\n",
    "macro_rules! members { () => { fn from_macro(&self) {} } }\npub struct Host;\nimpl Host { members!(); pub fn own(&self) {} }\n",
    "pub mod inner { pub fn deep() {} pub struct In; impl super::Tr for In {} impl In { pub fn a() {} pub fn b() {} } }\n",
    "pub fn free() { fn nested() {} }\npub const C: u8 = 0;\npub static S: u8 = 0;\npub type Alias = Always;\n",
    "pub struct Neg;\nimpl !Tr for Neg {}\n",
];

const SUB: &str = "pub struct InSub;\nimpl InSub { pub fn there() {} }\nimpl crate::Tr for InSub {}\n";

#[derive(Clone, Debug)]
struct Spec {
    parts: [bool; 12],
    /// A second file, `sub.rs`.
    sub: bool,
    /// An earlier crate whose trait this one implements.
    dep: Option<usize>,
}

fn draw(tape: &Tape) -> Vec<Spec> {
    let n = 1 + tape.index(3, "crates");
    let mut specs = Vec::new();
    for i in 0..n {
        let mut parts = [false; 12];
        for p in parts.iter_mut() {
            *p = tape.bool("part");
        }
        let dep = (i > 0 && tape.bool("depends")).then(|| tape.index(i, "dep"));
        specs.push(Spec { parts, sub: tape.bool("sub file"), dep });
    }
    specs
}

fn leak(s: String) -> &'static str {
    std::boxed::Box::leak(s.into_boxed_str())
}

fn text(spec: &Spec) -> &'static str {
    let mut t = String::from(ALWAYS);
    for (i, part) in PARTS.iter().enumerate() {
        if spec.parts[i] {
            t += part;
        }
    }
    if let Some(d) = spec.dep {
        t += &format!("pub struct ForTheirs;\nimpl {}::Tr for ForTheirs {{}}\n", CRATES[d]);
    }
    if spec.sub {
        t += "pub mod sub;\n";
    }
    leak(t)
}

fn world(specs: &[Spec]) -> (&'static Texts, Vec<CrateInput<'static>>) {
    let mut files = Vec::new();
    let mut inputs = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let root = leak(format!("/{}/lib.rs", CRATES[i]));
        let mut of_crate = std::vec![(root, text(s))];
        if s.sub {
            of_crate.push((leak(format!("/{}/sub.rs", CRATES[i])), SUB));
        }
        files.push(&*Vec::leak(of_crate));
        let deps: Vec<(&'static str, usize)> = s.dep.iter().map(|d| (CRATES[*d], *d)).collect();
        inputs.push(input(CRATES[i], root, Vec::leak(deps)));
    }
    (std::boxed::Box::leak(std::boxed::Box::new(Texts(Vec::leak(files)))), inputs)
}

/// The effect layer of the test: the records as written, the texts as the solve holds them,
/// and one file changed after the solve (`changed`: crate, path, the text it has now).
struct Loaded {
    records: Vec<&'static [u8]>,
    names: Vec<&'static CrateNames<'static>>,
    changed: Option<(usize, &'static str, &'static str)>,
}

impl LoadItems<'static> for Loaded {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        self.records.get(krate).copied()
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'static str> {
        let file = self.names.get(krate)?.files.iter().find(|f| f.package == path)?;
        match self.changed {
            Some((k, p, now)) if k == krate && p == file.path => Some(now),
            _ => Some(file.text),
        }
    }

    fn file_items(&self, krate: usize, index: u32, path: PackagePath<'_>, _: blaze_id::ContentId, edition: blaze_surface::Edition, alloc: blaze_own::Bump<'static>) -> Result<Option<CurrentFile<'static>>, ItemsRecordError> {
        let Some(file) = self.names.get(krate).and_then(|names| names.files.get(index as usize)) else { return Ok(None) };
        assert_eq!(file.package, path);
        if let Some((k, p, text)) = self.changed && k == krate && p == file.path {
            return CurrentFile::from_text(index, text, edition, alloc).map(Some);
        }
        CurrentFile::from_outline(index, file.text, &file.outline, alloc).map(Some)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'static str> {
        self.names.get(krate)?.expansions.get(&call).and_then(|e| e.made).map(|m| m.text)
    }
}

fn differs(what: String) -> Broke {
    Broke::Oracle(format!("the items record answers differently: {what}"))
}

/// A record's answer; an error where the fresh solve answers is a difference.
fn rec<T>(r: Result<T, ItemsRecordError>, what: &str) -> Result<T, Broke> {
    r.map_err(|e| differs(format!("{what}: the record says {e:?}")))
}

fn same<T: PartialEq + core::fmt::Debug>(fresh: T, record: T, what: &str) -> Result<(), Broke> {
    if fresh == record { Ok(()) } else { Err(differs(format!("{what}: fresh {fresh:?}, record {record:?}"))) }
}

/// Every question of part 1's table that the items record answers, asked of both.
fn ask(fresh: &Names<'static>, read: &RecordItems<'static, Loaded>, all: &[&'static CrateNames<'static>]) -> Result<(), Broke> {
    let g = Alloc::global();
    for (k, n) in all.iter().enumerate() {
        let c = n.name;
        let r = read.record(k).ok_or_else(|| Broke::Harness(format!("crate `{c}` has no items record")))?;
        // Questions 5, 6, 16, 17, 22: the crate's own facts.
        same(fresh.edition(k), r.edition(), &format!("`{c}`'s edition"))?;
        same(fresh.krate(k).cfg.atoms(), rec(r.cfg_atoms(), "cfg atoms")?, &format!("`{c}`'s cfg atoms"))?;
        same(n.implicit.proc_macro, r.proc_macro(), &format!("`{c}` is a proc-macro unit"))?;
        same(n.key, r.key(), &format!("`{c}`'s key"))?;
        same(fresh.root(k), r.root_module(), &format!("`{c}`'s root module"))?;
        same(n.proc_key, r.proc_key(), &format!("`{c}`'s proc key"))?;
        same(c, rec(r.name(), "name")?, "a crate's name")?;
        let marked = n.crate_root.features.contains(&"rustc_attrs") || n.files.first().is_none_or(|f| {
            f.outline.file_attrs().iter().any(|a| crate::task::strip_attr(a.range.of(f.text)) == "rustc_coherence_is_core")
        });
        same(marked, r.may_write_incoherent(), &format!("`{c}` may write incoherent impls"))?;

        // Question 19: every text, whole.
        same(n.files.len(), r.files(), &format!("`{c}`'s files"))?;
        for (i, f) in n.files.iter().enumerate() {
            let row = rec(r.file_surface(i as u32), "a file row")?.ok_or_else(|| differs(format!("`{c}` file {i}: no row")))?;
            let digest = blaze_id::ContentId::of("blaze v2 vfs file 1", f.text.as_bytes());
            same((f.package, f.shown, f.outline.surface()), (row.path, row.shown, row.surface), &format!("`{c}` file {i}'s surface"))?;
            let now = rec(read.whole(k, At::File(i as u32)), "a file's text")?;
            same((digest, f.text.len()), (blaze_id::ContentId::of("blaze v2 vfs file 1", now.as_bytes()), now.len()), &format!("`{c}` file {i}"))?;
            same(f.text, rec(read.whole(k, At::File(i as u32)), "a file's text")?, &format!("`{c}` file {i}'s text"))?;
        }
        let mut made = 0;
        for (call, e) in n.expansions.iter() {
            let Some(m) = e.made else { continue };
            made += 1;
            same(Some(m.text.len()), r.expansion(*call).map(|x| x.len as usize), &format!("`{c}`: the expansion of {call:?}"))?;
            same(m.text, rec(read.whole(k, At::Expansion(*call)), "an expansion's text")?, &format!("`{c}`: the text of the expansion of {call:?}"))?;
        }
        same(made, r.expansions(), &format!("`{c}`'s expansions"))?;

        // Questions 7 to 12 and 24: every item with text.
        let mut count = 0;
        for (key, module, place) in fresh.items(k) {
            count += 1;
            let it = fresh.item_text(key).ok_or_else(|| Broke::Harness(format!("item {key:?} has no text")))?;
            let row = rec(read.item(key), "an item")?.ok_or_else(|| differs(format!("`{c}` item {key:?} ({:?}): no row", it.name)))?;
            let what = format!("`{c}` item `{}`", it.name.unwrap_or("(unnamed)"));
            same(Some(module), rec(read.module_of(key), "an item's module")?, &format!("{what}: module"))?;
            same(it.name, row.name, &format!("{what}: name"))?;
            same(Some((place.at, place.item)), row.text.map(|s| (s.at, row.outline)), &format!("{what}: place"))?;
            same(Some(it.text), rec(read.item_text(key), "an item's text")?, &format!("{what}: text"))?;
            let (_, outline) = fresh.outline_at(place.krate, place.at).ok_or_else(|| Broke::Harness(format!("{what}: no outline")))?;
            let entry = outline.item(place.item);
            same(Some((entry.range.start, entry.range.len())), row.text.map(|s| (s.start, s.len)), &format!("{what}: byte range"))?;
            let pieces = rec(r.pieces(key), "item digests")?.ok_or_else(|| differs(format!("{what}: missing item digests")))?;
            same(entry.hash, pieces.body, &format!("{what}: body digest"))?;
            let detail = entry.detail;
            same(detail.kind(), row.kind, &format!("{what}: kind"))?;
            let holds = format!("{detail:?}").contains("body_holds_items: true");
            same(holds, row.body_holds_items, &format!("{what}: body holds items"))?;
            same(fresh.ctor(key).is_some(), row.ctor, &format!("{what}: is its own constructor"))?;
            let header = match row.impl_header {
                Some(span) => Some(rec(read.text(k, span), "an impl header")?),
                None => None,
            };
            same(it.impl_header, header, &format!("{what}: impl header"))?;
            // Question 13: its members, when it is a trait or an impl.
            let members = rec(read.members(key), "members")?;
            match (fresh.members(key), members) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    same(a.len(), b.len(), &format!("{what}: member list length"))?;
                    for (m, row) in a.iter().zip(b.iter()) {
                        let row = rec(row, "a member row")?;
                        let what = format!("{what}: member `{}`", m.name.unwrap_or("(unnamed)"));
                        same((m.key, m.name, m.item, m.detail.kind(), m.written_by), (row.key, row.name, row.outline, row.kind, row.written_by), &what)?;
                        same(format!("{:?}", m.detail).contains("body_holds_items: true"), row.body_holds_items, &format!("{what}: body holds items"))?;
                        same(m.text, rec(read.member_text(key, &row), "a member's text")?, &format!("{what}: text"))?;
                    }
                }
                (a, b) => return Err(differs(format!("{what}: members: fresh {:?}, record {:?}", a.map(<[_]>::len), b.map(|m| m.len())))),
            }
        }
        same(count, r.items(), &format!("`{c}`'s items"))?;
        let mut last: Option<ItemKey> = None;
        for i in 0..r.items() {
            let row = rec(r.item_at(i), "an item row")?.ok_or_else(|| differs(format!("`{c}`: item row {i} of {} is missing", r.items())))?;
            if last.is_some_and(|l| l >= row.key) {
                return Err(differs(format!("`{c}`: item rows are not sorted by key at row {i}")));
            }
            last = Some(row.key);
        }
        same(n.tables.members.len(), n.tables.members.keys().filter(|o| read.members(**o).is_ok_and(|m| m.is_some())).count(), &format!("`{c}`'s member owners"))?;

        // Question 14: the crate's impls, each with the facts of its header.
        // And a row per refused derive, its gap (D193 point 5), as the solve's values give it.
        let mut gaps: Vec<crate::items_record::ImplRow<'_>> = Vec::new();
        crate::items_record::each_refused_impl_row(n, &mut |row| gaps.push(row));
        same(n.tables.impls.len() + gaps.len(), r.impls(), &format!("`{c}`'s impl rows"))?;
        let mut before: Option<(&str, ItemKey, ItemKey)> = None;
        for i in 0..r.impls() {
            let row = rec(r.impl_at(i), "an impl row")?.ok_or_else(|| differs(format!("`{c}`: impl row {i} is missing")))?;
            if row.refused_call {
                if !gaps.contains(&row) {
                    return Err(differs(format!("`{c}`: impl row {i} is a refused derive's gap the solve's values do not give: {row:?}")));
                }
                let sort = (row.trait_path.unwrap_or(""), row.module, row.key);
                if before.is_some_and(|b| (b.0.as_bytes(), b.1, b.2) > (sort.0.as_bytes(), sort.1, sort.2)) {
                    return Err(differs(format!("`{c}`: impl rows are not sorted at row {i}")));
                }
                before = Some(sort);
                continue;
            }
            let src = n.tables.impls.iter().find(|s| s.key == row.key).ok_or_else(|| differs(format!("`{c}`: impl row {i} names {:?}, no impl of the solve", row.key)))?;
            let what = format!("`{c}` `{}`", src.header);
            same(src.module, row.module, &format!("{what}: module"))?;
            let h = blaze_file_items::impl_header(src.header, n.edition, g).map_err(|e| Broke::Harness(format!("{what}: {e}")))?;
            same(h.flags, row.flags, &format!("{what}: flags"))?;
            same(h.trait_.is_some(), row.has_trait, &format!("{what}: has a trait"))?;
            same(false, row.header_unread, &format!("{what}: header unread"))?;
            if let (Some(t), Some(path)) = (h.trait_, row.trait_path) {
                if !t.starts_with(path) {
                    return Err(differs(format!("{what}: trait path `{path}` does not begin `{t}`")));
                }
                same(t == path, row.bare, &format!("{what}: bare"))?;
            }
            let item = rec(read.item(row.key), "an impl's item")?;
            if let Some(item) = item {
                same(Some(i as u32), item.impl_row, &format!("{what}: the item row's impl row"))?;
            }
            let sort = (row.trait_path.unwrap_or(""), row.module, row.key);
            if before.is_some_and(|b| (b.0.as_bytes(), b.1, b.2) > (sort.0.as_bytes(), sort.1, sort.2)) {
                return Err(differs(format!("`{c}`: impl rows are not sorted at row {i}")));
            }
            before = Some(sort);
        }

        // Question 15: lang items by name.
        let mut names_of: Vec<&str> = n.tables.langs.iter().map(|l| l.name).collect();
        names_of.sort_unstable();
        names_of.dedup();
        for name in names_of.iter() {
            let first = n.tables.langs.iter().filter(|l| l.name == *name).map(|l| l.key).min();
            same(first, rec(r.lang(name), "a lang item")?, &format!("`{c}`: lang item `{name}`"))?;
        }
        same(None, rec(r.lang("no such lang item"), "a lang item")?, &format!("`{c}`: a lang item nobody declares"))?;
        let mut pairs: Vec<(&str, ItemKey)> = n.tables.langs.iter().map(|l| (l.name, l.key)).collect();
        pairs.sort_unstable();
        pairs.dedup();
        same(pairs.len(), r.langs(), &format!("`{c}`'s lang rows"))?;

        // Question 21: every macro definition, body text included.
        for (def, _) in n.tables.macros.iter() {
            let a = fresh.macro_src(*def);
            let b = rec(read.macro_src(*def), "a macro definition")?;
            same(a.map(|m| format!("{m:?}")), b.map(|m| format!("{m:?}")), &format!("`{c}`: macro {def:?}"))?;
        }

        // Questions 8 and 20: every module, and that an item's key is no module row's twin.
        let mut listed = 0;
        for m in fresh.modules(k) {
            listed += 1;
            let row = rec(read.module(m.key), "a module")?.ok_or_else(|| differs(format!("`{c}` module `{}`: no row", m.path)))?;
            same((m.parent, Some(m.path), true), (row.parent, row.path, row.listed), &format!("`{c}` module `{}`", m.path))?;
            same(n.tables.texts.contains_key(&m.key), rec(read.item(m.key), "an item")?.is_some(), &format!("`{c}` module `{}` as an item", m.path))?;
        }
        let mut rows = 0;
        for i in 0..r.modules() {
            if rec(r.module_at(i), "a module row")?.is_some_and(|m| m.listed) {
                rows += 1;
            }
        }
        same(listed, rows, &format!("`{c}`'s listed modules"))?;
    }
    Ok(())
}

/// A file changed under the record: any item of it must be refused as stale, never answered
/// with its old range of the new text.
fn stale(tape: &Tape, records: &[&'static [u8]], all: &[&'static CrateNames<'static>], keys: &[CrateItemKey]) -> Result<(), Broke> {
    let bump = super::SyncFrame::leaked();
    let k = tape.index(all.len(), "changed crate");
    let n = all[k];
    let file = tape.index(n.files.len(), "changed file");
    let f = n.files[file];
    // A new outline entry changes the file's surface and must refuse the old names record.
    let now = leak(format!("{}\nfn added_after_solve() {{}}\n", f.text));
    let read = RecordItems::new(keys, Loaded { records: records.to_vec(), names: all.to_vec(), changed: Some((k, f.path, now)) }, bump);
    let mut items: Vec<ItemKey> = n.tables.texts.iter().filter(|(_, t)| t.1 == At::File(file as u32)).map(|(key, _)| *key).collect();
    items.sort_unstable();
    let Some(&item) = items.first() else { return Ok(()) };
    for round in 0..2 {
        match read.item_text(item) {
            Err(ItemsRecordError::Stale { file: Some(at), saw, found, .. }) if at == file as u32 && saw == f.outline.surface() && found != f.outline.surface() => {}
            Ok(text) => return Err(Broke::Oracle(format!("a changed file was read through the stale record (ask {round}): `{}` gave {text:?}", f.path))),
            Err(e) => return Err(Broke::Oracle(format!("a changed file was read through the stale record (ask {round}): not refused as stale but as {e:?}"))),
        }
    }
    // The other crates' texts are still answered.
    for (other, o) in all.iter().enumerate() {
        if other != k {
            if let Some((key, t)) = o.tables.texts.iter().next() {
                same(Some(t.0.text), rec(read.item_text(*key), "an item's text beside a stale file")?, "an unchanged crate's item beside a stale file")?;
            }
        }
    }
    Ok(())
}

fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let specs = draw(tape);
    let (texts, inputs) = world(&specs);
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).map_err(|e| Broke::Harness(format!("solve: {e:?}")))?;
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).map_err(|e| Broke::Harness(format!("names: {e:?}")))?;
    PLANT.with(|p| p.set(plant));
    let out = (|| {
        let mut records: Vec<&'static [u8]> = Vec::new();
        for n in all.iter() {
            let a = encode(n, g).map_err(|e| Broke::Harness(format!("encode `{}`: {e:?}", n.name)))?;
            let b = encode(n, g).map_err(|e| Broke::Harness(format!("encode `{}` again: {e:?}", n.name)))?;
            if a != b {
                return Err(differs(format!("`{}` encodes to different bytes twice", n.name)));
            }
            records.push(aligned(&a, bump));
        }
        let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
        let read = RecordItems::new(&keys, Loaded { records: records.clone(), names: all.clone(), changed: None }, bump);
        ask(&fresh, &read, &all)?;
        stale(tape, &records, &all, &keys)
    })();
    PLANT.with(|p| p.set(Plant::None));
    out
}

/// The property: the record's answers are a fresh solve's, and a changed file is refused.
#[test]
fn the_items_record_answers_as_a_fresh_solve() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, Plant::None));
}

/// Planted: the first impl row left out of the record.
#[test]
fn a_dropped_impl_row_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::DropImpl, "answers differently", case);
}

/// Planted: one member list written one short.
#[test]
fn a_member_list_cut_by_one_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::ShortMembers, "answers differently", case);
}

/// Planted: the reader hands out a text without checking its digest.
#[test]
fn a_file_changed_under_the_record_is_refused_as_stale() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::SkipDigest, "read through the stale record", case);
}

#[test]
fn a_body_only_edit_keeps_the_items_record_bytes() {
    let tail = "\nstruct S; impl S { fn g() {} } macro_rules! make { () => { struct Made; } } make!();\n";
    let before = solve_one(leak(format!("#![no_core]\nfn f() {{ 1 }}{tail}")), None);
    let after = solve_one(leak(format!("#![no_core]\nfn f() {{ let rustc_attrs = 100; rustc_attrs }}{tail}")), None);
    assert_eq!(before.files[0].outline.surface(), after.files[0].outline.surface());
    assert_ne!(before.files[0].text.len(), after.files[0].text.len());
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let g = bump.alloc();
    let first = encode(before, g).unwrap_or_else(|e| panic!("encode before: {e:?}"));
    let second = encode(after, g).unwrap_or_else(|e| panic!("encode after: {e:?}"));
    assert_eq!(first, second, "a body-only edit changed the items record bytes");
    let record = one_file(aligned(&first, bump), after.files[0].text, bump);
    for (&key, (item, at)) in after.tables.texts.iter() {
        if !matches!(at, At::File(_)) { continue; }
        let row = record.item(key).expect("read joined row").expect("present item");
        let span = row.text.expect("file item's range");
        assert_eq!(record.slice(span, &|_| None).expect("current file slice"), item.text);
        assert_eq!(record.pieces(key).expect("read pieces").expect("body hash").body, after.files[0].outline.item(row.outline).hash);
    }
    for (&key, src) in after.tables.macros.iter() {
        let restored = record.macro_src(key, &|_| None).expect("the macro reads").expect("the macro is present");
        assert_eq!(format!("{:?}", restored.body), format!("{:?}", src.body));
    }
}

/// Open a single-file fixture with the same indexed join used by a multi-file loader.
pub(super) fn one_file<'a>(bytes: &'a [u8], text: &'a str, alloc: blaze_own::Bump<'a>) -> ItemsRecord<'a> {
    ItemsRecord::open_with_files(bytes, alloc.alloc(), move |index, _, _, edition| {
        assert_eq!(index, 0);
        CurrentFile::from_text(index, text, edition, alloc).map(Some)
    }).expect("open the single-file fixture")
}

pub(super) fn solve_one(text: &'static str, sub: Option<&'static str>) -> &'static CrateNames<'static> {
    let g = super::SyncFrame::leaked();
    let mut of_crate = std::vec![("/k0/lib.rs", text)];
    of_crate.extend(sub.map(|s| ("/k0/sub.rs", s)));
    let texts: &'static Texts = std::boxed::Box::leak(std::boxed::Box::new(Texts(Vec::leak(std::vec![&*Vec::leak(of_crate)]))));
    let inputs = std::vec![input("k0", "/k0/lib.rs", &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("solve: {e:?}"));
    solved[0].names
}

/// The drawn crates' parts fill every table the record is made from: without this the
/// property could pass over empty tables.
#[test]
fn the_drawn_parts_fill_every_table() {
    let spec = Spec { parts: [true; 12], sub: true, dep: None };
    let n = solve_one(text(&spec), Some(SUB));
    assert!(n.files.len() == 2, "files: {}", n.files.len());
    assert!(n.tables.impls.len() >= 14, "impls: {}", n.tables.impls.len());
    assert!(n.tables.members.values().any(|m| m.len() >= 3), "no owner with three members");
    assert!(n.tables.members.values().flat_map(|m| m.iter()).any(|m| m.written_by.is_some()), "no member written by a macro call");
    assert!(!n.tables.macros.is_empty(), "no macro definition");
    assert!(!n.tables.langs.is_empty(), "no lang item");
    assert!(!n.tables.ctors.is_empty(), "no constructor");
    assert!(n.expansions.values().any(|e| e.made.is_some()), "no expansion made a text");
    assert!(n.tables.texts.values().any(|t| matches!(t.1, At::Expansion(_))), "no item in an expansion");
    assert!(!n.made.is_empty() || n.tree.modules.len() >= 3, "modules: {}", n.tree.modules.len());
}

/// What an impl row says of each header, stated by hand from the Reference, "Implementations"
/// ("impl<..> Type" is an inherent impl; "impl<..> !? Trait for Type" a trait impl) and
/// "Types" (the forms of a type expression: a path, `&`, `&mut`, `*const`, `*mut`, `[T]`,
/// `[T; N]`, `!`, a tuple, a trait object, a function pointer).
/// Each struct, enum, union, trait, alias and associated type row holds the lifetimes its own
/// generics declare (D155), read back in place by key (and an associated type by its trait
/// and name); a fn's and an impl's rows hold none. Control (rule 23): a fn row answers `None`,
/// not `Some(0)`, so the oracle's `Some(0)` rows are counted, not a default.
#[test]
fn item_rows_say_their_declared_lifetimes() {
    const TEXT: &str = "#![no_core]
pub struct Z;
pub struct One<'a>(&'a u8);
pub struct Mixed<T, 'a, const N: usize, 'b>(&'a T, &'b [u8; N]);
pub struct Def<T, A = Z>(T, A);
pub enum E<'x> { A(&'x u8) }
pub union U<'u> { a: &'u u8 }
pub trait Tr<'t> { type Gat<'g, 'h>; type Plain; fn f(&self); }
pub type Al<'p, T> = Mixed<T, 'p, 1, 'p>;
pub fn free<'f>(x: &'f u8) -> &'f u8 { x }
impl<'i> Tr<'i> for Z { type Gat<'g, 'h> = u8; type Plain = u8; fn f(&self) {} }
";
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let n = solve_one(TEXT, None);
    let bytes = aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump);
    let read = RecordItems::new(&[n.key], Loaded { records: std::vec![bytes], names: std::vec![n], changed: None }, bump);
    let r = read.record(0).unwrap_or_else(|| panic!("no record"));
    let key_of = |name: &str| -> ItemKey {
        (0..r.items())
            .filter_map(|i| r.identity_at(i).unwrap_or_else(|e| panic!("item row {i}: {e:?}")))
            .find(|it| it.name == Some(name))
            .map(|it| it.key)
            .unwrap_or_else(|| panic!("`{name}` has no item row"))
    };
    let lives = |name: &str| r.declared_lifetimes(key_of(name)).unwrap_or_else(|e| panic!("`{name}`'s lifetimes: {e:?}"));
    for (name, want) in [("Z", 0), ("One", 1), ("Mixed", 2), ("Def", 0), ("E", 1), ("U", 1), ("Tr", 1), ("Al", 1)] {
        assert_eq!(lives(name), Some(want), "ORACLE: `{name}`'s declared lifetimes");
    }
    assert_eq!(lives("free"), None, "CONTROL: a fn's row holds a count");
    let tr = key_of("Tr");
    let assoc = |name: &str| r.assoc_lifetimes(tr, |m| m == name).unwrap_or_else(|e| panic!("`Tr::{name}`: {e:?}"));
    assert_eq!(assoc("Gat"), Some(2), "ORACLE: `Tr::Gat`'s own lifetimes");
    assert_eq!(assoc("Plain"), Some(0), "ORACLE: `Tr::Plain`'s own lifetimes");
    assert_eq!(assoc("f"), None, "CONTROL: a fn member answered as an associated type");
    // The solved crate answers alike, from the same texts (the source over solved names).
    for name in ["Z", "One", "Mixed", "Def", "E", "U", "Tr", "Al", "free"] {
        let solved = crate::items_record::solved_lifetimes(n, key_of(name)).unwrap_or_else(|e| panic!("`{name}` solved: {e:?}"));
        assert_eq!(solved, lives(name), "the solved names and the record disagree on `{name}`");
    }
    for name in ["Gat", "Plain", "f"] {
        let solved = crate::items_record::solved_assoc_lifetimes(n, tr, |m| m == name).unwrap_or_else(|e| panic!("`Tr::{name}` solved: {e:?}"));
        assert_eq!(solved, assoc(name), "the solved names and the record disagree on `Tr::{name}`");
    }
}

#[test]
fn impl_rows_say_what_each_header_writes() {
    const TEXT: &str = "#![no_core]
pub struct S;
pub struct G<T>(T);
pub trait Tr {}
pub trait Gen<T> {}
pub mod m { pub trait Deep {} }
impl S {}
impl<T> G<T> {}
impl Tr for S {}
impl<T> Gen<T> for S {}
impl<T> Tr for T {}
impl m::Deep for &S {}
impl m::Deep for &mut S {}
impl<'a> Tr for &'a mut G<u8> {}
impl Tr for *const S {}
impl Tr for *mut S {}
impl Tr for [S] {}
impl Tr for [S; 2] {}
impl Tr for () {}
impl<A, B> Tr for (A, B) {}
impl<A> Tr for (A,) {}
impl Tr for dyn Gen<u8> {}
impl Tr for fn(S) -> S {}
impl !Tr for G<S> {}
unsafe impl<T> Gen<[T; 2]> for m::Other<T> {}
";
    // header, trait path, bare, head, self path, negative.
    let want: [(&str, Option<&str>, bool, SelfHead, Option<&str>, bool); 19] = [
        ("impl S", None, false, SelfHead::Path, Some("S"), false),
        ("impl<T> G<T>", None, false, SelfHead::Path, Some("G"), false),
        ("impl Tr for S", Some("Tr"), true, SelfHead::Path, Some("S"), false),
        ("impl<T> Gen<T> for S", Some("Gen"), false, SelfHead::Path, Some("S"), false),
        ("impl<T> Tr for T", Some("Tr"), true, SelfHead::Param, Some("T"), false),
        ("impl m::Deep for &S", Some("m::Deep"), true, SelfHead::Ref, None, false),
        ("impl m::Deep for &mut S", Some("m::Deep"), true, SelfHead::RefMut, None, false),
        ("impl<'a> Tr for &'a mut G<u8>", Some("Tr"), true, SelfHead::RefMut, None, false),
        ("impl Tr for *const S", Some("Tr"), true, SelfHead::PtrConst, None, false),
        ("impl Tr for *mut S", Some("Tr"), true, SelfHead::PtrMut, None, false),
        ("impl Tr for [S]", Some("Tr"), true, SelfHead::Slice, None, false),
        ("impl Tr for [S; 2]", Some("Tr"), true, SelfHead::Array, None, false),
        ("impl Tr for ()", Some("Tr"), true, SelfHead::Tuple(0), None, false),
        ("impl<A, B> Tr for (A, B)", Some("Tr"), true, SelfHead::Tuple(2), None, false),
        ("impl<A> Tr for (A,)", Some("Tr"), true, SelfHead::Tuple(1), None, false),
        ("impl Tr for dyn Gen<u8>", Some("Tr"), true, SelfHead::Other, None, false),
        ("impl Tr for fn(S) -> S", Some("Tr"), true, SelfHead::Other, None, false),
        ("impl !Tr for G<S>", Some("Tr"), true, SelfHead::Path, Some("G"), true),
        ("unsafe impl<T> Gen<[T; 2]> for m::Other<T>", Some("Gen"), false, SelfHead::Path, Some("m::Other"), false),
    ];
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let n = solve_one(TEXT, None);
    let bytes = aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump);
    let read = RecordItems::new(&[n.key], Loaded { records: std::vec![bytes], names: std::vec![n], changed: None }, bump);
    let r = read.record(0).unwrap_or_else(|| panic!("no record"));
    assert!(r.impls() == want.len(), "{} impl rows for {} impls", r.impls(), want.len());
    let mut inherent_done = false;
    for i in 0..r.impls() {
        let row = r.impl_at(i).unwrap_or_else(|e| panic!("impl row {i}: {e:?}")).unwrap_or_else(|| panic!("impl row {i} missing"));
        // Inherent impls (no trait path) sort before every trait impl.
        if row.trait_path.is_some() {
            inherent_done = true;
        } else {
            assert!(!inherent_done, "an inherent impl after a trait impl at row {i}");
        }
        let item = read.item(row.key).unwrap_or_else(|e| panic!("item of impl row {i}: {e:?}")).unwrap_or_else(|| panic!("impl row {i} has no item"));
        let span = item.impl_header.unwrap_or_else(|| panic!("impl row {i}: its item has no header"));
        let header = read.text(0, span).unwrap_or_else(|e| panic!("header of impl row {i}: {e:?}")).trim();
        let w = want.iter().find(|w| w.0 == header).unwrap_or_else(|| panic!("no expectation for the header `{header}`"));
        assert!(!row.header_unread && !row.trait_unread, "`{header}`: unread");
        assert!(row.trait_path == w.1, "`{header}`: trait path {:?}, want {:?}", row.trait_path, w.1);
        assert!(row.has_trait == w.1.is_some(), "`{header}`: has_trait {}", row.has_trait);
        assert!(row.bare == w.2, "`{header}`: bare {}, want {}", row.bare, w.2);
        assert!(row.head == w.3, "`{header}`: head {:?}, want {:?}", row.head, w.3);
        assert!(row.self_path == w.4, "`{header}`: self path {:?}, want {:?}", row.self_path, w.4);
        assert!(row.flags.contains(ImplFlags::NEGATIVE) == w.5, "`{header}`: negative {}", row.flags.contains(ImplFlags::NEGATIVE));
        assert!(row.flags.contains(ImplFlags::UNSAFE) == header.starts_with("unsafe"), "`{header}`: unsafe");
    }
}
