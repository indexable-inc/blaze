//! An expansion's origin column is opened ONCE per records, however often its marks are asked
//! (front2 source.rs `Records::text_marks`, rule 74): every later ask hands out the view the
//! first opened, O(1).
//!
//! Rule 23, the cause removed: before this piece `Records::text_marks` called
//! `item_site::marks_of` on every ask, which opens the column (blaze-expand origin_column.rs
//! `OriginsRef::open`: every mark and row read), so this test's 500 asks were 500 opens and
//! `Opened::origins` reads 500 with the `marks` lookup taken out.
//!
//! The records are a real solve's (one crate, one `macro_rules!` call): `text_marks` reads the
//! text through the items record's expansion row and its digest check, as the product does, so
//! a loader that serves an expansion no items record names is refused there.

#![feature(allocator_api)]

use blaze_expand::origin_column::{InputShape, OriginColumn, TextMark, TextOrigin};
use blaze_expand::{DefSite, Edition, Transparency};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::crates::syntax_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::expansion_origins::{CallInText, write};
use blaze_scope::graph::Sysroot;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{At, CrateInput, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemIndex;
use blaze_unit_graph::implicit;
use edition::PackageEdition;

const MARKS: usize = 2_000;
const ASKS: usize = 500;

/// The one crate: its one macro call writes an item, so its items record keeps the call's
/// expansion row (the call key, the text's digest), as every product records' reader reads it.
const ROOT: &str = "#![feature(no_core)]\n#![no_core]\n\nmacro_rules! m { () => { pub struct Written; }; }\n\nm!();\n";
const ROOT_PATH: &str = "/app/src/lib.rs";

struct Root;

impl<'t> SourceTexts<'t> for Root {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        (krate == 0 && path == ROOT_PATH).then_some(ROOT)
    }
}

/// One crate's records and its one macro call's expansion `text`, served with `origins`: the
/// solve's names and items records, the call's text as the solve made it (its digest is the
/// items row's), and the large origin column this test asks the marks of.
struct OneExpansion<'n> {
    names: &'n [u8],
    items: &'n [u8],
    /// The root file as the records name it (its package path) and its text.
    file: (&'n str, &'n str),
    call: ItemKey,
    text: &'n str,
    origins: &'n [u8],
}

impl<'n> Load<'n> for OneExpansion<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        (krate == 0).then_some(self.names)
    }
    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        (krate == 0).then_some(self.items)
    }
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'n str> {
        (krate == 0 && path.as_str() == self.file.0).then_some(self.file.1)
    }
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'n>> {
        if krate != 0 || call != self.call {
            return None;
        }
        // The fixture keeps the joined form `write` made: its two parts, cut where the column
        // starts.
        let origins = blaze_scope::expansion_origins::split(self.origins).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"));
        Some(LoadedExpansion { text: self.text, origins: Some(origins) })
    }
}

/// The origins of `call`: `MARKS` one-byte rows two bytes apart, each written by a definition
/// with a mark of its own (a module key per row), so the column keeps `MARKS` rows and `MARKS`
/// marks and its open reads them all (the cost this test proves is paid once). Its rows are
/// not those of the solve's text: the column is opened and read by row index, never against
/// the text's bytes (front2 source.rs `Records::text_marks`, blaze-expand origin_column.rs
/// `OriginsRef::open`).
fn origins_of(call: ItemKey, bump: blaze_own::Bump<'static>) -> &'static [u8] {
    let mut column = OriginColumn::new_in(bump.alloc());
    for i in 0..MARKS {
        let start = 2 * i;
        let mut module = [0u8; 32];
        module[..4].copy_from_slice(&(i as u32).to_be_bytes());
        let mark = TextMark {
            site: DefSite { krate: CrateItemKey(ContentId([7; 32])), module: Some(ItemKey(ContentId(module))) },
            transparency: Transparency::SemiOpaque,
            edition: Edition::E2021,
        };
        column.push(start, start + 1, TextOrigin::Def(i as u32), Some(mark)).expect("rows in order");
    }
    let place = CallInText { text: Some(At::File(0)), item: ItemIndex(0), input_start: 0, input_len: 0 };
    write(call, Some(place), InputShape::Inside, "m", false, &column, bump).expect("the origins' bytes")
}

/// `ROOT` solved: its names and items records, its root file, and its one macro call with the
/// text the solve made for it.
fn solved(bump: blaze_own::Bump<'static>) -> (CrateItemKey, OneExpansion<'static>) {
    let alloc = bump.alloc();
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let key = CrateItemKey(ContentId::of("front2_text_marks_once crate", b"app"));
    let crates = [CrateInput {
        name: "app",
        key,
        edition: syntax_of(rust),
        implicit: implicit(rust, &blaze_unit_graph::UnitKind::Lib),
        root: ROOT_PATH,
        shown_from: "/app/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    }];
    let names = resolve_crates(&crates, Sysroot::default(), &Root, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let krate = names.crates()[0];
    let mut made = krate.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, m.text)));
    let (call, text) = made.next().expect("`m!()` made a text");
    assert!(made.next().is_none(), "the crate's one macro call is its one expansion");
    let file = krate.files.first().expect("the crate's root file");
    let names_bytes = names_record::encode(krate, alloc).unwrap_or_else(|e| panic!("the names record: {e:?}"));
    let items_bytes = items_record::encode(krate, alloc).unwrap_or_else(|e| panic!("the items record: {e:?}"));
    let one = OneExpansion {
        names: aligned(&names_bytes, bump),
        items: aligned(&items_bytes, bump),
        file: (std::string::String::leak(file.package.as_str().to_string()), file.text),
        call,
        text,
        origins: origins_of(call, bump),
    };
    (key, one)
}

#[test]
fn an_expansion_asked_five_hundred_times_is_opened_once() {
    // The test's memory: one frame, handed down (leaked, as the records tests' are, so the
    // records may be `&'static`).
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
    let bump = erased.bump();
    let alloc: Alloc<'static> = bump.alloc();
    let (key, one) = solved(bump);
    let (call, text) = (one.call, one.text);
    let load: &'static OneExpansion<'static> = Box::leak(Box::new(one));
    let keys: &'static [CrateItemKey] = Box::leak(Box::new([key]));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(&["app"], alloc)));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, alloc)));
    let source = records;
    assert_eq!(records.opened().origins, 0, "nothing opened before the first ask");
    for ask in 0..ASKS {
        let marks = source.text_marks(0, call).unwrap_or_else(|e| panic!("ask {ask}: {e}")).expect("the text keeps a column");
        assert_eq!(marks.text, text);
        // The handed-out view reads the mark the column holds (a row's, by its index).
        let row = ask * (MARKS / ASKS);
        let mark = marks.column.mark(row).expect("every row has a mark");
        let mut module = [0u8; 32];
        module[..4].copy_from_slice(&(row as u32).to_be_bytes());
        assert_eq!(mark.site.module, Some(ItemKey(ContentId(module))), "row {row}'s mark");
    }
    assert_eq!(records.opened().origins, 1, "one open for {ASKS} asks of one expansion");
    // A call the loader does not hold is refused by name, and opens nothing.
    let other = ItemKey(ContentId([4; 32]));
    assert!(source.text_marks(0, other).is_err());
    assert_eq!(records.opened().origins, 1);
    std::println!("text marks: {ASKS} asks of one expansion of {MARKS} marks, 1 column opened");
}
