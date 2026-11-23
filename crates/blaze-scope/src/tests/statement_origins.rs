//! An item written by a macro call in STATEMENT position has the call's origins (lane N5): the
//! items text a statement call's expansion is read as (crate::expanded_blocks
//! `statements_as_items`) keeps a call part (the call's text, item, input, shape and macro) and
//! the column moved with it, as the text of an item-position call does; so an item there walks
//! up to the call that wrote it (blaze-cli body_types/covered.rs `written_path`).
//!
//! The shapes: a `thread_local!`-like call (`static KEY: u8 = 1; let _ = &KEY;`: a static beside
//! a `let`) and a `macro_rules!`-defined struct beside a statement (`struct Point { .. } let _v
//! = 0;`), each called in a fn body; and, beside them, an item-position call (`top!(Top)`), whose
//! text always had its call part.

use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Erased};

use super::entry::input;
use crate::expansion_origins::OriginsView;
use crate::graph::Sysroot;
use crate::resolve::{SourceTexts, resolve_crates};
use crate::task::Serial;

const ROOT: &str = "/stmt/lib.rs";

const STATEMENT_CALLS: &str = "#![no_core]
macro_rules! local_key { ($n:ident) => { static $n: u8 = 1; let _ = &$n; } }
macro_rules! point { ($n:ident) => { struct $n { x: u8 } let _v: u8 = 0; } }
macro_rules! top { ($n:ident) => { pub struct $n; } }
top!(Top);
pub fn f() { local_key!(KEY); point!(Point); }
";

struct One;

impl<'a> SourceTexts<'a> for One {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == ROOT).then_some(STATEMENT_CALLS)
    }
}

/// Expected line: `test tests::statement_origins::a_statement_calls_items_have_the_calls_origins
/// ... ok`, printing `STATEMENT ORIGINS: local_key: call part names `local_key`, rows N (marks
/// read: true); point: call part names `point`, rows M (marks read: true); top (item position):
/// call part names `top`, rows T`. CONTROL: on the tree before this piece the two statement
/// calls' texts hold `Err(Rewritten { what: "a call's statements read as items" })`, and the
/// test fails at "origins of `local_key`" printing that error; the item-position call `top!`
/// passes on both trees.
#[test]
fn a_statement_calls_items_have_the_calls_origins() {
    let frame = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let g = bump.alloc();
    let crates = [input("stmt", ROOT, &[])];
    let names = resolve_crates(&crates, Sysroot::default(), &One, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let krate = names.krate(0);
    let mut made = AVec::new_in(g);
    made.extend(krate.expansions.values().filter_map(|e| e.made));
    // The expansion whose text declares `needle`, its origins opened.
    let origins_of = |needle: &str, macro_name: &str| {
        let text = made.iter().find(|m| m.text.contains(needle)).unwrap_or_else(|| {
            for m in made.iter() {
                std::println!("STATEMENT ORIGINS: a text: `{}`", m.text);
            }
            panic!("an expansion declares `{needle}` ({} texts, printed above)", made.len())
        });
        let bytes = text.origins.unwrap_or_else(|e| panic!("origins of `{macro_name}`: {e:?} (text `{}`)", text.text));
        let view = OriginsView::open(bytes).unwrap_or_else(|e| panic!("the origins of `{macro_name}` open: {e:?}"));
        assert_eq!(view.macro_name, macro_name, "the call part names the macro as written");
        assert!(view.column.len() != 0, "`{macro_name}`'s column places the text's tokens");
        (view.column.len(), text.column.is_ok())
    };
    // Found by what each expansion DECLARES (rendered a space between tokens): the calling
    // block's module text holds the calls as written (`local_key!(KEY);`), never their items,
    // and its origins are rightly `Rewritten` (covered.rs stops at a block key first).
    let (key_rows, key_marks) = origins_of("static KEY", "local_key");
    let (point_rows, point_marks) = origins_of("struct Point", "point");
    let (top_rows, _) = origins_of("struct Top", "top");
    std::println!(
        "STATEMENT ORIGINS: local_key: call part names `local_key`, rows {key_rows} (marks read: {key_marks}); point: call part names `point`, rows {point_rows} (marks read: {point_marks}); top (item position): call part names `top`, rows {top_rows}"
    );
    assert!(key_marks && point_marks, "the moved column is read back as the text's marks");
}
