use super::head::{FreezeError, Frozen, FrozenHead, freeze, import};
use super::head_tests::{a, world_of};
use crate::front2::head_record::{self, Head, HeadAt, HeadRecordError};
use crate::front2::heads::Want;
use crate::front2::keys::checker_of;
use crate::front2::receiver::Pointers;
use crate::front2::source::NamesSource;

fn freeze_f(app: &'static str) -> (Frozen<'static>, HeadAt, &'static str) {
    let (names, sized, frame) = world_of(app);
    let mut found = None;
    NamesSource::each_item(names, 0, &mut |key, _| {
        if NamesSource::name_of(names, key) == Some("f") {
            found = Some(key);
        }
    });
    let key = found.expect("the app declares f");
    let text = NamesSource::text_of(names, key).expect("the current text of f").text;
    let frozen = freeze(names, sized, Pointers::NOT_LOOKED_UP, Want::Fn(key), frame, a(), a()).unwrap_or_else(|e| panic!("freeze f: {e}"));
    (frozen, HeadAt::Fn(checker_of(key)), text)
}

fn refuses_text(frozen: &FrozenHead<'_>, at: HeadAt, text: &str) {
    let tys = crate::front2::rows::stable_tys(a()).expect("fresh type table");
    let result = import(frozen, &mut |k| (k == at).then_some(text), &tys, a(), a());
    match result {
        Err(FreezeError::Record { want, at: refused, error: HeadRecordError::Text { key, .. } }) => {
            assert_eq!(want, frozen.want());
            assert_eq!(refused, at);
            assert_eq!(HeadAt::Fn(key), at);
        }
        Err(e) => panic!("expected a refusal naming f's text, got {e}"),
        Ok(_) => panic!("import accepted a change to f's recorded text"),
    }
}

#[test]
fn a_body_edit_imports_the_fresh_head_with_todays_text() {
    // The first brace in the text is in a comment. Only the parser's block starts the body.
    let (before, at, _) = freeze_f("#![no_std]\npub fn f(/* { */ x: u8) -> u8 { x }");
    let (after, now_at, today) = freeze_f("#![no_std]\npub fn f(/* { */ x: u8) -> u8 { let y = x; y }");
    assert_eq!(at, now_at);
    assert_eq!(&before.bytes[..], &after.bytes[..], "a body-only edit preserves all frozen head bytes");
    let frozen = FrozenHead::read(&before.bytes).expect("old head");
    let fresh = FrozenHead::read(&after.bytes).expect("fresh head");
    let tys = crate::front2::rows::stable_tys(a()).expect("fresh type table");
    let mut current_text = |k| (k == now_at).then_some(today);
    let reused = import(&frozen, &mut current_text, &tys, a(), a()).unwrap_or_else(|e| panic!("import after body edit: {e}"));
    let new = import(&fresh, &mut current_text, &tys, a(), a()).unwrap_or_else(|e| panic!("import fresh head: {e}"));
    let [(here, head @ Head::Fn { head: row, .. })] = &reused[..] else { panic!("expected f's head") };
    let [(there, new_head)] = &new[..] else { panic!("expected one fresh head") };
    assert_eq!((*here, *there), (at, at));
    assert_eq!(row.text_prefix, Some("pub fn f(/* { */ x: u8) -> u8 ".len() as u32));
    assert_eq!(row.text, "pub fn f(/* { */ x: u8) -> u8 { let y = x; y }");
    assert_eq!(head_record::same(head.as_ref(), new_head.as_ref(), tys.view()), None);
    refuses_text(&frozen, at, "pub fn f(/* { */ x: i8) -> u8 { 0 }");
    refuses_text(&frozen, at, "pub fn f(/* { */ x: u8) -> u8 where u8: Copy { x }");
    std::println!("body edit imports the fresh head with today's full text; signature edits refuse f's text");
}

#[test]
fn const_and_static_heads_refuse_initializer_edits() {
    for (app, edited) in [
        ("#![no_std]\npub const f: u8 = 1;", "pub const f: u8 = 2;"),
        ("#![no_std]\npub static f: u8 = 1;", "pub static f: u8 = 2;"),
    ] {
        let (before, at, text) = freeze_f(app);
        let frozen = FrozenHead::read(&before.bytes).expect("body-keyed head");
        assert_eq!(text.len(), edited.len(), "the digest must catch the edit");
        refuses_text(&frozen, at, edited);
        // The full old text is still a prefix: the whole-text length must be exact.
        refuses_text(&frozen, at, &std::format!("{text} "));
    }
    std::println!("const and static heads refuse edited initializer text");
}
