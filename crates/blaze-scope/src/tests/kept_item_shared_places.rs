//! OC (2026-11-17): a kept item whose text has tokens SHARING one place keeps a column that
//! reads.
//!
//! tempo's build census g051231 (tree 847c0fc770): 70 rows "an expansion's origin row 1 (0..39)
//! is out of order after one ending at 39", every one the head of a type written
//! `/// doc` + `#[derive(..)] #[helper(..)]` (alloy's `CallKind`, alloy-eip7928's
//! `AccountChanges`, commonware's `U64`, tempo's `SignatureType`, protobuf's `Error`): the lexer
//! places a doc comment's tokens `# [ doc = ".." ]` all at the comment's bytes (and a lifetime's
//! `'` and name at the lifetime's), and expansion_origins.rs `kept` pushed one row per token.
//! Now one row a place (blaze-expand `OriginColumn::input_identity`).

use std::vec::Vec;

use blaze_expand::origin_column::{InputShape, Mapped, TextOrigin, input_places, map_range};
use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallRole, Edition};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::expansion_origins::OriginsView;
use crate::graph::Sysroot;
use crate::resolve::{CrateInput, SourceTexts, resolve_crates};
use crate::task::Serial;

const MACROS: &str = "#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show(item: TokenStream) -> TokenStream { item }
";

/// The three shapes in one kept text: a doc comment first (its six tokens at one place), a
/// group right after it (`#[show(..)]`'s brackets and parentheses), and a lifetime (`'a`: two
/// tokens at one place) in the head and a field.
const USER: &str = "#![no_core]
pub trait Shown {}
/// The kind of a call, as a tracer names it.
#[derive(macros::Show)]
#[show(rename = \"s\")]
pub struct Named<'a> {
    name: &'a u8,
}
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([14; 32]));

struct WithRunner(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::Derive, Some("Show")) => "impl<'a> Shown for Named<'a> {}",
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[&[("/user/lib.rs", USER)], &[("/macros/lib.rs", MACROS)]]);

/// Rule: the kept text of `Named` keeps origins that open (sorted, each place once), one row a
/// distinct place of its tokens, each row the FIRST token of its place with no mark; the doc
/// comment and the lifetime map back to their own bytes in the file.
///
/// Expected: "1 passed". Before OC the origins were `Err(Column { error: Unsorted { row: 1,
/// start: 0, .. } })` (the doc comment's `#` and `[` both at its bytes) and the first assertion
/// fails with that error.
#[test]
fn a_kept_item_with_a_doc_comment_and_a_lifetime_keeps_a_column_that_reads() {
    let bump = super::SyncFrame::leaked();
    let alloc = bump.alloc();
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    let keep = decls.calls.iter().find(|c| c.role == CallRole::Keep).unwrap_or_else(|| panic!("`Named` has a keep call: notes {:?}", names.notes(0)));
    let made = user.expansions.get(&keep.call).and_then(|e| e.made).unwrap_or_else(|| panic!("the keep call made its text: notes {:?}", names.notes(0)));
    let bytes = made.origins.unwrap_or_else(|e| panic!("the kept item's text keeps origins: {e}"));
    let view = OriginsView::open(bytes).unwrap_or_else(|e| panic!("the origins open: {e}"));
    assert!(made.text.starts_with("/// The kind"), "the kept text starts at the doc comment: {:?}", made.text);
    // One row a distinct place, the first token's, in order.
    let places = input_places(made.text, InputShape::Whole, Edition::E2021, alloc).expect("the kept text lexes");
    let mut want: Vec<(u32, u32, TextOrigin)> = Vec::new();
    for (i, &(start, end)) in places.iter().enumerate() {
        if want.last().is_none_or(|w| (w.0, w.1) != (start, end)) {
            want.push((start, end, TextOrigin::Input(i as u32)));
        }
    }
    assert!(want.len() < places.len(), "the text has tokens sharing a place: {} places", places.len());
    let rows: Vec<(u32, u32, TextOrigin)> = (0..view.column.len()).map(|i| view.column.row(i)).map(|r| (r.start, r.end, r.origin)).collect();
    assert_eq!(rows, want, "one row a place, its first token's");
    assert!((0..view.column.len()).all(|i| view.column.mark(i).is_none()), "a kept text's tokens keep no mark");
    // A range maps to the user's own bytes: the doc comment, and `'a` of the field.
    let input_at = names.outline(0, 0).expect("the root file's outline").item(view.item).range.start + view.input_start;
    let written = &USER[input_at as usize..(input_at + view.input_len) as usize];
    let written_places = input_places(written, InputShape::Whole, Edition::E2021, alloc).expect("the call's input lexes");
    let user_bytes = |needle: &str, from: usize| {
        let at = (from + made.text[from..].find(needle).expect("the kept text holds the needle")) as u32;
        let Mapped::Input { first, last } = map_range(view.column, at, at + needle.len() as u32) else { panic!("`{needle}` is input") };
        let (start, end) = (input_at + written_places[first as usize].0, input_at + written_places[last as usize].1);
        &USER[start as usize..end as usize]
    };
    assert_eq!(user_bytes("/// The kind of a call, as a tracer names it.", 0), "/// The kind of a call, as a tracer names it.");
    let field = made.text.find("name:").expect("the field");
    assert_eq!(user_bytes("'a", field), "'a", "the field's lifetime maps to its bytes");
    assert_eq!(user_bytes("#[show(rename = \"s\")]", 0), "#[show(rename = \"s\")]", "the helper attribute's group maps to its bytes");
}
