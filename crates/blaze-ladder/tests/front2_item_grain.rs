//! After an edit inside ONE body, front2 over the crate's OLD records and the file's NEW text
//! checks the edited body, and reads the other bodies of the file at their new places
//! (front2/source/item_grain.rs; RECORDS-PIPELINE.md "Runner 2", gap (b)).
//!
//! A program of one file with `main` and two fns, `first` then `second`, is solved once and
//! each crate's names and items records are written from that solve, as tests/front2_records.rs
//! writes them; the solved names are dropped. Then three readers over THE SAME RECORDS, each
//! handed another text of `src/main.rs`:
//!
//! 1. the text the records were written from (`BEFORE`): `first` and `second` check clean
//!    (no diagnostic), and the log is empty: the baseline;
//! 2. `first`'s BODY edited (`BODY_EDIT`): longer, so `second` starts later in the file, and
//!    with a planted type error: `let wrong: bool = x;` where `x: u32`. The Reference, "Let
//!    statements": "If a type annotation is present, the initializer expression ... must have
//!    that type", and `u32` is not `bool` (no coercion between them: the Reference, "Type
//!    coercions", lists none). The edited body is REPORTED with at least one diagnostic (the
//!    proof it was checked over the new text and not skipped), `second` is checked at its new
//!    place with the same outcome as in (1) (positions in a check are item-relative, rule 71),
//!    the log is empty, and its count of whole-file stale refusals (`Log::stale_files`) is 0;
//! 3. `second`'s SIGNATURE edited (`SIG_EDIT`: it returns `u64`, a type-only edit its crate's
//!    `Surface` leaves out): its row is FRESH, so its text is read as it is today (`-> u64`),
//!    no item is refused and `first` still reads; again no whole-file refusal. (Until the D114
//!    ruling the row held a head and `second` was refused as `ItemStale` while the scope stood
//!    valid by its reads, which nothing then remade: the class this step guards.)
//!
//! WITHOUT THE FIX (source.rs's `text_of` reading through `ItemsRecord::whole`): reader 2's
//! `first` is "not lowered" with the log holding "the items record of crate `hello_world`
//! (crate 0): Stale { .. }", so `BODY: edited body reported` panics with "the records filed
//! [..Stale..]" (and `stale_files` would be 2 or more); reader 3 files a whole-file `Stale`,
//! never reads `second` as edited.
//!
//! It should print:
//! `ITEM GRAIN: before: first clean, second clean`
//! `ITEM GRAIN: body edit: first reported with <n> diagnostics, second clean and equal at its new place; whole-file stale refusals 0`
//! `ITEM GRAIN: signature edit: second fresh at item grain, read as edited, first still read; item refusals 0, whole-file stale refusals 0`
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{self, BuildOrder, CheckFail, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records, SourceError};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::Front2;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};

/// The program's `Cargo.toml` edition line.
const EDITION: &str = "blaze-2026.11";

/// `src/main.rs` as the records were written from it.
const BEFORE: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}

fn first() -> u32 {
    1
}

fn second() -> u32 {
    2
}
";

/// `first`'s body edited (longer, with a planted type error); every byte after it shifts.
const BODY_EDIT: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}

fn first() -> u32 {
    let x: u32 = 10;
    let wrong: bool = x;
    x
}

fn second() -> u32 {
    2
}
";

/// `second`'s signature edited, in type only: its row is fresh.
const SIG_EDIT: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}

fn first() -> u32 {
    1
}

fn second() -> u64 {
    2
}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve: the test's effect layer.
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    /// This loader keeps texts, not their origins: a finding in one is filed `NotServed`.
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        Some(LoadedExpansion { text, origins: None })
    }
}

impl Kept {
    /// The same records with the program's `src/main.rs` (crate 0's text equal to `BEFORE`)
    /// handed in as `now`.
    fn with_main(&self, now: &'static str) -> Kept {
        let mut files = self.files.clone();
        let mut swapped = 0;
        for text in files[0].values_mut() {
            if *text == BEFORE {
                *text = now;
                swapped += 1;
            }
        }
        assert_eq!(swapped, 1, "crate 0 holds `src/main.rs` once");
        Kept { names: self.names.clone(), items: self.items.clone(), files, expansions: self.expansions.clone() }
    }
}

/// A reader over `kept` alone (leaked: a `Records` borrows itself for its whole life).
fn reader(keys: &'static [CrateItemKey], crate_names: &'static [&'static str], kept: Kept) -> (&'static Records<'static>, &'static blaze_mem::Frame) {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, Alloc::global())));
    let load: &'static Kept = Box::leak(Box::new(kept));
    (Box::leak(Box::new(Records::new(keys, load, log, frame, Alloc::global()))), frame)
}

/// A body's check: its outcome as text and how many diagnostics it reported (`None`: the check
/// did not finish).
struct Outcome {
    text: String,
    diags: Option<usize>,
}

/// Each fn of `fns` lowered and checked over a fresh front end on `records`.
fn check_all(records: &'static Records<'static>, frame: &'static blaze_mem::Frame, lang_crates: &[usize], fns: &[ItemKey]) -> Vec<Outcome> {
    let mut front = Front2::from_records(records, frame, lang_crates).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut store, mut buckets, mut consts) = (Store::new(front.index.lang("sized"), frame, blaze_own::Alloc::global()), None, Consts::default());
    let mut out = Vec::new();
    for key in fns.iter() {
        let body = match lower_fn(records, prepared, &mut front.keys, &mut store, *key) {
            Ok(body) => body,
            Err(e) => {
                out.push(Outcome { text: format!("not lowered: {e}"), diags: None });
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = match Spelling::of(&view) {
            Ok(spell) => spell,
            Err(e) => {
                out.push(Outcome { text: format!("not spelled: {e:?}"), diags: None });
                continue;
            }
        };
        let scope = body.scope(&spell);
        let mut r = check::Records { names: records, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        out.push(match check_body(&mut r, &body, &scope) {
            Ok(done) => Outcome { text: format!("checked: {done:?}"), diags: Some(done.diags.len()) },
            // A check that finished with errors is returned as `CheckFail::Diags` (check.rs).
            Err(CheckFail::Diags { owner, diags, filed: _ }) => Outcome { text: format!("reported: {owner:?} {diags:?}"), diags: Some(diags.len()) },
            Err(e @ CheckFail::Gap { .. }) => Outcome { text: format!("gaps: {e:?}"), diags: None },
            Err(e) => Outcome { text: format!("not checked: {e:?}"), diags: None },
        });
    }
    out
}

fn errors(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

#[test]
fn an_edited_body_is_checked_over_the_old_records() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BEFORE.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let core = position("core").expect("a crate named core");
    let lang_crates: Vec<usize> = [Some(0), Some(core), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();

    // The solve and the records written from it; nothing of the solved names leaves the block.
    let g = Alloc::global();
    let (kept, first, second) = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let root = names.root(0);
        let key = |name: &str| Paths { names: &names }.path(root, false, &[name], Ns::Value).unwrap_or_else(|e| panic!("`{name}` from the crate root: {e}")).key;
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode_with(c, &names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        }
        (kept, key("first"), key("second"))
    };
    let fns = [first, second];

    // 1. The text the records saw: the baseline.
    let (before, before_frame) = reader(keys, crate_names, kept.with_main(BEFORE));
    let base = check_all(before, before_frame, &lang_crates, &fns);
    assert!(errors(before).is_empty(), "before: the records filed {:?}", errors(before));
    assert_eq!(base[0].diags, Some(0), "before: `first` checks clean: {}", base[0].text);
    assert_eq!(base[1].diags, Some(0), "before: `second` checks clean: {}", base[1].text);
    println!("ITEM GRAIN: before: first clean, second clean");

    // 2. `first`'s body edited, the records unchanged.
    let (edited, edited_frame) = reader(keys, crate_names, kept.with_main(leak(BODY_EDIT)));
    let now = check_all(edited, edited_frame, &lang_crates, &fns);
    assert!(errors(edited).is_empty(), "body edit: the records filed {:?}", errors(edited));
    assert_eq!(edited.log().stale_files(), 0, "body edit: no whole-file stale refusal");
    let text = edited.text_of(first).expect("`first`'s text over the edited file");
    assert!(text.text.contains("let wrong: bool = x;"), "body edit: `first` is read from TODAY's text, not the record's offsets: {:?}", text.text);
    let reported = now[0].diags.unwrap_or_else(|| panic!("body edit: `first`'s check finished: {}", now[0].text));
    assert!(reported >= 1, "body edit: the planted type error in `first` is reported: {}", now[0].text);
    let second_text = edited.text_of(second).expect("`second`'s text over the edited file");
    assert!(second_text.text.starts_with("fn second() -> u32"), "body edit: `second` is read at its new place: {:?}", second_text.text);
    assert_eq!(now[1].text, base[1].text, "body edit: `second` checks as before at its new place");
    println!("ITEM GRAIN: body edit: first reported with {reported} diagnostics, second clean and equal at its new place; whole-file stale refusals {}", edited.log().stale_files());

    // 3. `second`'s signature edited in type only: fresh, read as it is today.
    let (sig, _) = reader(keys, crate_names, kept.with_main(leak(SIG_EDIT)));
    let second_text = sig.text_of(second).unwrap_or_else(|| panic!("signature edit: `second` is fresh and reads: {:?}", errors(sig)));
    assert!(second_text.text.starts_with("fn second() -> u64"), "signature edit: `second` is read from TODAY's text: {:?}", second_text.text);
    let named = sig.errors().iter().any(|e| matches!(e, SourceError::ItemStale { .. }));
    assert!(!named, "signature edit: no item is refused as stale: {:?}", errors(sig));
    assert_eq!(sig.log().stale_items(), 0, "signature edit: no item refused: {:?}", errors(sig));
    let first_text = sig.text_of(first).expect("signature edit: `first` still reads");
    assert!(first_text.text.starts_with("fn first() -> u32"), "signature edit: `first` read: {:?}", first_text.text);
    assert_eq!(sig.log().stale_files(), 0, "signature edit: no whole-file stale refusal");
    println!(
        "ITEM GRAIN: signature edit: second fresh at item grain, read as edited, first still read; item refusals {}, whole-file stale refusals {}",
        sig.log().stale_items(),
        sig.log().stale_files()
    );
}
