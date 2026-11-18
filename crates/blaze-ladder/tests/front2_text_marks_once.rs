//! An expansion's origin column is opened ONCE per records, however often its marks are asked
//! (front2 source.rs `Records::text_marks`, rule 74): every later ask hands out the view the
//! first opened, O(1).
//!
//! Rule 23, the cause removed: before this piece `Records::text_marks` called
//! `item_site::marks_of` on every ask, which opens the column (blaze-expand origin_column.rs
//! `OriginsRef::open`: every mark and row read), so this test's 500 asks were 500 opens and
//! `Opened::origins` reads 500 with the `marks` lookup taken out.

#![feature(allocator_api)]

use blaze_expand::origin_column::{InputShape, OriginColumn, TextMark, TextOrigin};
use blaze_expand::{DefSite, Edition, Transparency};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::expansion_origins::{CallInText, write};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;

const MARKS: usize = 2_000;
const ASKS: usize = 500;

/// One crate whose one macro call expanded to `text`, with `origins`.
struct OneExpansion<'n> {
    call: ItemKey,
    text: &'n str,
    origins: &'n [u8],
}

impl<'n> Load<'n> for OneExpansion<'n> {
    fn names(&self, _: usize) -> Option<&'n [u8]> {
        None
    }
    fn items(&self, _: usize) -> Option<&'n [u8]> {
        None
    }
    fn file(&self, _: usize, _: PackagePath<'_>) -> Option<&'n str> {
        None
    }
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'n str> {
        (krate == 0 && call == self.call).then_some(self.text)
    }
    fn expansion_origins(&self, krate: usize, call: ItemKey) -> Option<(&'n [u8], &'n [u8])> {
        if krate != 0 || call != self.call {
            return None;
        }
        // The fixture keeps the joined form `write` made: its two parts, cut where the column
        // starts.
        Some(blaze_scope::expansion_origins::split(self.origins).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}")))
    }
}

/// `MARKS` tokens `x` separated by spaces, each written by a definition with a mark of its own
/// (a module key per token), so the column keeps `MARKS` rows and `MARKS` marks.
fn expansion(call: ItemKey, alloc: Alloc<'static>) -> (&'static str, &'static [u8]) {
    let mut text = std::string::String::new();
    let mut column = OriginColumn::new_in(alloc);
    for i in 0..MARKS {
        let start = text.len();
        text.push_str("x ");
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
    let origins = write(call, Some(place), InputShape::Inside, "m", false, &column, alloc).expect("the origins' bytes");
    (std::string::String::leak(text), origins)
}

#[test]
fn an_expansion_asked_five_hundred_times_is_opened_once() {
    // The test's memory: one frame, handed down (leaked, as the records tests' are, so the
    // records may be `&'static`).
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
    let alloc: Alloc<'static> = erased.handle();
    let call = ItemKey(ContentId([3; 32]));
    let (text, origins) = expansion(call, alloc);
    let load: &'static OneExpansion<'static> = Box::leak(Box::new(OneExpansion { call, text, origins }));
    let keys: &'static [CrateItemKey] = Box::leak(Box::new([CrateItemKey(ContentId([1; 32]))]));
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
