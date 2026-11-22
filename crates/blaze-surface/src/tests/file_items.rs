//! `FileItems`' promises: a record reads back every value an outline holds, name for name and
//! attribute for attribute, its children walk the tree the parents say, and an entry whose
//! parent, descendants or ranges are not where they are said to be is refused.

use std::alloc::Global;

use blaze_format::Aligned;
use blaze_id::ContentId;
use blaze_sim::{Broke, Config, Tape};

use super::{file, name, small};
use crate::error::BuildError;
use crate::file_items::{
    Attr, AttrKind, AttrSpan, ErrorOrigin, FileItems, FileItemsView, Item, ItemDetail, ItemFlags,
    SyntaxError,
};
use crate::kind::{ItemIndex, Range, WrittenVis};
use crate::path::Rename;

/// A range inside an entry `len` bytes long.
fn range_in(tape: &Tape, len: u32) -> Range {
    let start = small(tape, len + 1, "range start");
    Range::new(start, start + small(tape, len - start + 1, "range len"))
}

fn maybe(tape: &Tape, len: u32, label: &'static str) -> Option<Range> {
    tape.coin(label).then(|| range_in(tape, len))
}

fn detail(tape: &Tape, len: u32) -> ItemDetail {
    match tape.below(9, "detail") {
        0 => ItemDetail::Fn { body_holds_items: tape.coin("holds items") },
        1 => ItemDetail::Mod { inline: tape.coin("inline"), path: maybe(tape, len, "path") },
        2 => ItemDetail::ExternCrate {
            rename: match tape.below(3, "rename") {
                0 => Rename::Same,
                1 => Rename::To(range_in(tape, len)),
                _ => Rename::Underscore,
            },
        },
        3 => ItemDetail::MacroCall { path: maybe(tape, len, "path"), input: maybe(tape, len, "input") },
        4 => ItemDetail::Field { ty: maybe(tape, len, "ty"), default: maybe(tape, len, "default") },
        5 => ItemDetail::Unhandled { syntax: u16::try_from(small(tape, 300, "syntax")).expect("below 300") },
        6 => ItemDetail::Use { tree: maybe(tape, len, "tree") },
        7 => ItemDetail::MacroRules { exported: tape.coin("exported") },
        _ => ItemDetail::Impl,
    }
}

fn visibility(tape: &Tape, len: u32) -> WrittenVis<Range> {
    match tape.below(7, "visibility") {
        0 => WrittenVis::Private,
        1 => WrittenVis::Pub,
        2 => WrittenVis::Crate,
        3 => WrittenVis::Super,
        4 => WrittenVis::SelfOnly,
        5 => WrittenVis::In(range_in(tape, len)),
        _ => WrittenVis::WithoutIn(range_in(tape, len)),
    }
}

fn attr(tape: &Tape, len: u32) -> Attr {
    let kind = match tape.below(6, "attr kind") {
        0 => AttrKind::Doc,
        1 => AttrKind::Cfg,
        2 => AttrKind::CfgAttr,
        3 => AttrKind::Path { literal: range_in(tape, len) },
        4 => AttrKind::MacroExport,
        _ => AttrKind::Other,
    };
    Attr { range: range_in(tape, len), inner: tape.coin("inner"), kind }
}

/// `attr` as an outline writes it: a cfg attribute's bytes (from `base`, its entry's start, in
/// `text`) are whole characters, as every attribute an outline finds is; one drawn across a
/// character covers its entry's `len` bytes instead.
fn whole_chars(attr: Attr, text: &str, base: u32, len: u32) -> Attr {
    let cuts = attr.range.shifted(base).get(text).is_none();
    match (attr.kind, cuts) {
        (AttrKind::Cfg | AttrKind::CfgAttr, true) => Attr { range: Range::new(0, len), ..attr },
        _ => attr,
    }
}

/// The text each cfg attribute of `span` keeps, as `drawn` wrote it from `base`.
fn cfg_texts<'t>(drawn: &'t Drawn, span: AttrSpan, base: u32) -> Vec<(AttrKind, &'t str), Global> {
    let mut texts = Vec::new_in(Global);
    for attr in drawn.attrs[span.to_usize()].iter() {
        if matches!(attr.kind, AttrKind::Cfg | AttrKind::CfgAttr) {
            texts.push((attr.kind, attr.range.shifted(base).of(&drawn.text)));
        }
    }
    texts
}

fn id(tape: &Tape, label: &'static str) -> ContentId {
    ContentId([u8::try_from(small(tape, 256, label)).expect("below 256"); 32])
}

/// One file's entries, drawn: a tree in preorder (each entry's descendants follow it, up to its
/// `end`), each entry's text its name then filler, and attributes and errors beside them.
struct Drawn {
    text: String,
    items: Vec<Item, Global>,
    attrs: Vec<Attr, Global>,
    file_attrs: AttrSpan,
    errors: Vec<SyntaxError, Global>,
    messages: Vec<u8, Global>,
    file_flags: ItemFlags,
}

fn drawn(tape: &Tape) -> Drawn {
    let mut text = String::new();
    let mut items: Vec<Item, Global> = Vec::new_in(Global);
    let mut attrs = Vec::new_in(Global);
    let mut open: Vec<u32, Global> = Vec::new_in(Global);
    let count = small(tape, 12, "entries");
    for index in 0..count {
        for _ in 0..small(tape, u32::try_from(open.len()).expect("at most 12") + 1, "closed") {
            let closed = open.pop().expect("closes at most the open entries");
            items[usize::try_from(closed).expect("a u32")].end = ItemIndex(index);
        }
        let start = u32::try_from(text.len()).expect("a small text");
        let named = tape.coin("named");
        let written = name(tape);
        if named {
            text.push_str(written);
        }
        for _ in 0..small(tape, 40, "filler") {
            text.push('.');
        }
        let len = u32::try_from(text.len()).expect("a small text") - start;
        let attrs_at = u32::try_from(attrs.len()).expect("few attributes");
        for _ in 0..small(tape, 3, "attributes") {
            attrs.push(whole_chars(attr(tape, len), &text, start, len));
        }
        let attrs_end = u32::try_from(attrs.len()).expect("few attributes");
        let name_len = u32::try_from(written.len()).expect("a short name");
        items.push(Item {
            detail: detail(tape, len),
            parent: open.last().copied().map(ItemIndex),
            end: ItemIndex(count),
            range: Range::new(start, start + len),
            name: named.then_some(Range::new(0, name_len)),
            visibility: visibility(tape, len),
            signature: range_in(tape, len),
            body: maybe(tape, len, "body"),
            attrs: AttrSpan { start: attrs_at, end: attrs_end },
            disambiguator: small(tape, 4, "disambiguator"),
            hash: id(tape, "hash"),
            signature_hash: id(tape, "signature hash"),
            flags: flags(tape),
        });
        open.push(index);
    }
    let text_len = u32::try_from(text.len()).expect("a small text");
    let file_at = u32::try_from(attrs.len()).expect("few attributes");
    for _ in 0..small(tape, 3, "file attributes") {
        attrs.push(whole_chars(attr(tape, text_len), &text, 0, text_len));
    }
    let file_attrs = AttrSpan { start: file_at, end: u32::try_from(attrs.len()).expect("few attributes") };
    let (mut errors, mut messages) = (Vec::new_in(Global), Vec::new_in(Global));
    for _ in 0..small(tape, 3, "errors") {
        let start = u32::try_from(messages.len()).expect("short messages");
        messages.extend_from_slice(name(tape).as_bytes());
        let message = Range::new(start, u32::try_from(messages.len()).expect("short messages"));
        let origin = match tape.coin("lexer") {
            true => ErrorOrigin::Lexer,
            false => ErrorOrigin::Parser,
        };
        errors.push(SyntaxError { range: range_in(tape, text_len), origin, message });
    }
    let file_flags = flags(tape);
    Drawn { text, items, attrs, file_attrs, errors, messages, file_flags }
}

impl Drawn {
    fn parts(&self) -> FileItems<'_> {
        FileItems {
            file: file(0),
            text: &self.text,
            items: &self.items,
            attrs: &self.attrs,
            file_attrs: self.file_attrs,
            errors: &self.errors,
            messages: &self.messages,
            file_flags: self.file_flags,
        }
    }
}

fn flags(tape: &Tape) -> ItemFlags {
    let bits = u8::try_from(tape.below(16, "flags")).expect("a draw below 16 fits a u8");
    ItemFlags::from_bits(bits).expect("four bits are every flag")
}

fn oracle(what: &str) -> Broke {
    Broke::Oracle(what.into())
}

/// Refinement: every value of a drawn file reads back as written, and each entry's
/// children are exactly the entries whose parent it is.
#[test]
fn file_items_read_back_every_value() {
    blaze_sim::check(Config::cases(300), |tape| {
        let drawn = drawn(tape);
        let bytes = drawn.parts().write(blaze_own::testing::heap().alloc()).map_err(|error| Broke::Oracle(error.to_string()))?;
        let buffer = Aligned::copy_from(&bytes);
        let view = FileItemsView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
        if view.file() != file(0) || !view.items().map(|entry| entry.item).eq(drawn.items.iter().copied()) {
            return Err(oracle("a file's entries read back other than written"));
        }
        for entry in view.items() {
            let name = entry.item.name.map_or("", |name| entry.text(&drawn.text, name));
            if entry.name != name {
                return Err(oracle("an entry's name read back other than its text"));
            }
            if !view.attrs(&entry).eq(drawn.attrs[entry.attrs.to_usize()].iter().copied()) {
                return Err(oracle("an entry's attributes read back other than written"));
            }
            if !view.cfgs(Some(&entry.item)).eq(cfg_texts(&drawn, entry.attrs, entry.range.start).into_iter()) {
                return Err(oracle("an entry's cfg attributes' texts read back other than its own bytes"));
            }
        }
        if !view.cfgs(None).eq(cfg_texts(&drawn, drawn.file_attrs, 0).into_iter()) {
            return Err(oracle("the file's cfg attributes' texts read back other than its own bytes"));
        }
        if !view.file_attrs().eq(drawn.attrs[drawn.file_attrs.to_usize()].iter().copied())
            || !view.errors().eq(drawn.errors.iter().copied())
        {
            return Err(oracle("the file's attributes or errors read back other than written"));
        }
        // The roll-up: the file's flags are its own and every entry's, and the flagged entries
        // are exactly those with a flag.
        let all = drawn.items.iter().fold(drawn.file_flags, |flags, item| flags | item.flags);
        let flagged = drawn.items.iter().enumerate().filter(|(_, item)| !item.flags.is_empty());
        let flagged = flagged.map(|(at, item)| (ItemIndex(u32::try_from(at).expect("few")), item.flags));
        if view.flags() != all
            || view.own_flags() != drawn.file_flags
            || !view.flagged().eq(flagged.clone())
            || crate::flags_of_files([&view, &view]) != all
            || !crate::flagged_in_files([&view]).map(|(_, index, flags)| (index, flags)).eq(flagged)
        {
            return Err(oracle("a file's flags roll up other than its entries'"));
        }
        for error in &drawn.errors {
            if view.message(error).as_bytes() != &drawn.messages[error.message.to_usize()] {
                return Err(oracle("an error's message read back other than written"));
            }
        }
        let parents = (0..drawn.items.len()).map(|at| Some(ItemIndex(u32::try_from(at).expect("few"))));
        for parent in core::iter::once(None).chain(parents) {
            let written = drawn.items.iter().enumerate().filter(|(_, item)| item.parent == parent);
            let written = written.map(|(at, _)| ItemIndex(u32::try_from(at).expect("few")));
            if !view.children(parent).map(|(index, _)| index).eq(written) {
                return Err(Broke::Oracle(format!("the children of {parent:?} are not the entries it parents")));
            }
        }
        Ok(())
    });
}

/// WHAT [`AttrRow`] 2 COSTS (D191: a cfg attribute's text for plan time): no byte a row (its
/// text is in the `literal` slot version 1 left zero), and a file with no `#[cfg]` or
/// `#[cfg_attr]` keeps no byte of text: its names table is its entries' names, as before. The
/// control: the same file with its cfg attributes kept holds exactly their bytes more.
///
/// [`AttrRow`]: crate::file_items::AttrRow
#[test]
fn a_cfg_attributes_text_is_the_only_byte_it_adds() {
    assert_eq!(core::mem::size_of::<crate::file_items::AttrRow>(), 20, "two ranges, `inner`, the kind and 2 bytes of padding: version 1's size");
    blaze_sim::check(Config::cases(200), |tape| {
        let mut drawn = drawn(tape);
        let names: usize = drawn.items.iter().map(|item| item.name.map_or(0, |name| usize::try_from(name.len()).expect("a u32"))).sum();
        let texts: usize = drawn
            .items
            .iter()
            .map(|item| cfg_texts(&drawn, item.attrs, item.range.start).iter().map(|(_, text)| text.len()).sum::<usize>())
            .sum::<usize>()
            + cfg_texts(&drawn, drawn.file_attrs, 0).iter().map(|(_, text)| text.len()).sum::<usize>();
        let names_len = |drawn: &Drawn| -> Result<usize, Broke> {
            let bytes = drawn.parts().write(blaze_own::testing::heap().alloc()).map_err(|error| Broke::Oracle(error.to_string()))?;
            let buffer = Aligned::copy_from(&bytes);
            Ok(FileItemsView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?.names_len())
        };
        if names_len(&drawn)? != names + texts {
            return Err(oracle("a file's names table is not its names and its cfg attributes' texts"));
        }
        for attr in drawn.attrs.iter_mut() {
            if matches!(attr.kind, AttrKind::Cfg | AttrKind::CfgAttr) {
                attr.kind = AttrKind::Other;
            }
        }
        if names_len(&drawn)? != names {
            return Err(oracle("a file with no cfg attribute keeps a byte more than its names"));
        }
        Ok(())
    });
}

/// `fn f() {}` (or any one-letter name) as one entry.
pub(super) const F: Item = Item {
    detail: ItemDetail::Fn { body_holds_items: false },
    parent: None,
    end: ItemIndex(1),
    range: Range::new(0, 9),
    name: Some(Range::new(3, 4)),
    visibility: WrittenVis::Private,
    signature: Range::new(0, 6),
    body: Some(Range::new(7, 9)),
    attrs: AttrSpan { start: 0, end: 0 },
    disambiguator: 0,
    hash: ContentId([3; 32]),
    signature_hash: ContentId([4; 32]),
    flags: ItemFlags::NONE,
};

fn write_one(item: Item) -> Result<(), BuildError> {
    let parts = FileItems {
        file: file(0),
        text: "fn f() {}",
        items: &[item],
        attrs: &[],
        file_attrs: AttrSpan { start: 0, end: 0 },
        errors: &[],
        messages: &[],
        file_flags: ItemFlags::NONE,
    };
    parts.write(blaze_own::testing::heap().alloc()).map(|_| ())
}

/// The control is written; each fault, planted in it alone, is refused by name.
#[test]
fn an_entry_out_of_place_or_past_itself_is_refused() {
    assert_eq!(write_one(F), Ok(()));
    assert_eq!(write_one(Item { body: Some(Range::new(7, 10)), ..F }), Err(BuildError::PastItem { index: 0 }));
    assert!(write_one(Item { range: Range::new(0, 20), ..F }).is_ok(), "a range past the text is not the record's to refuse");
    assert_eq!(
        write_one(Item { range: Range::new(7, 16), ..F }),
        Err(BuildError::PastItem { index: 0 }),
        "a name past the text is",
    );
    assert!(matches!(write_one(Item { parent: Some(ItemIndex(0)), ..F }), Err(BuildError::BadIndex { index: 0, .. })));
    assert!(matches!(write_one(Item { end: ItemIndex(0), ..F }), Err(BuildError::BadIndex { index: 0, .. })));
    assert!(matches!(write_one(Item { end: ItemIndex(2), ..F }), Err(BuildError::BadIndex { index: 0, .. })));
    assert!(matches!(
        write_one(Item { attrs: AttrSpan { start: 0, end: 1 }, ..F }),
        Err(BuildError::BadIndex { table: "FileItems attrs", .. })
    ));
}
