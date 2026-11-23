//! KI (2026-11-17): a kept item's text keeps its origins, the entry its call is at first.
//!
//! tempo's `blaze check` (port27/tempo-deps/check-g043851.out): 132 items not checked, each "a
//! head it reads was written by a macro of a workspace crate at no entry a `Signature` covers":
//! every head is an ADT written `#[derive(..)] #[helper(..)] struct S { .. }` (serde's
//! `#[serde(..)]`, thiserror's `#[error(..)]`), which the derives' helper attributes keep through
//! a keep call (resolve.rs `keep_item`). That call's text kept no origins (`Rewritten`), so no
//! reader could walk from the head to the file entry the call is at, and no `Signature` covered it.

use blaze_expand::origin_column::{InputShape, Mapped, TextOrigin, input_places, map_range};
use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallRole, Edition, ItemDetail};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::expansion_origins::OriginsView;
use crate::graph::Sysroot;
use crate::resolve::{At, CrateInput, SourceTexts, resolve_crates};
use crate::task::Serial;

/// A derive declaring the helper `show` (serde's `Serialize` and `#[serde(..)]`).
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show(item: TokenStream) -> TokenStream { item }
";

const USER: &str = "#![no_core]
pub trait Shown {}
#[derive(macros::Show)]
#[show(rename = \"s\")]
pub struct S {
    a: u8,
    #[show(skip)]
    b: u16,
}
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([13; 32]));

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
            (ProcKind::Derive, Some("Show")) => "impl Shown for S {}",
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[&[("/user/lib.rs", USER)], &[("/macros/lib.rs", MACROS)]]);

/// Rule: the keep call of `S` (its derive's helper `#[show(..)]` keeps it) writes origins whose
/// call part names the root file and the outline entry of `S` itself, and whose column is the
/// identity: token `i` of the kept text is input token `i`, so the field type `u16` maps back to
/// its own bytes in the file.
///
/// Expected: "1 passed, 0 failed". Before KI the origins were `Err(Rewritten { what: "a kept
/// item's text" })` and the first assertion fails with that error.
#[test]
fn a_kept_item_s_origins_name_its_entry() {
    let bump = super::SyncFrame::leaked();
    let alloc = bump.alloc();
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    let keep = decls.calls.iter().find(|c| c.role == CallRole::Keep).unwrap_or_else(|| panic!("`S` has a keep call: notes {:?}", names.notes(0)));
    let made = user.expansions.get(&keep.call).and_then(|e| e.made).unwrap_or_else(|| panic!("the keep call made its text: notes {:?}", names.notes(0)));
    let bytes = made.origins.unwrap_or_else(|e| panic!("the kept item's text keeps origins: {e}"));
    let view = OriginsView::open(bytes).unwrap_or_else(|e| panic!("the origins open: {e}"));
    // The call part: the root file, the entry `S` is, the whole item.
    assert_eq!(view.text, At::File(0), "the keep call is written in the root file");
    assert_eq!(Some(view.item.0), keep.item, "its entry is the item the keep call is on");
    let outline = names.outline(0, 0).expect("the root file's outline");
    let entry = outline.item(view.item);
    assert!(matches!(entry.detail, ItemDetail::Struct), "the entry is the struct `S`: {:?}", entry.detail);
    assert_eq!(view.shape, InputShape::Whole);
    // The column: the identity over the kept text's tokens.
    let places = input_places(made.text, InputShape::Whole, Edition::E2021, alloc).expect("the kept text lexes");
    assert_eq!(view.column.len(), places.len(), "a row a token");
    for (i, &(start, end)) in places.iter().enumerate() {
        let row = view.column.row(i);
        assert_eq!((row.start, row.end, row.origin, row.mark), (start, end, TextOrigin::Input(i as u32), None), "row {i}");
    }
    // A range in the kept text maps to the user's bytes: `u16` of field `b`.
    let at = made.text.find("u16").expect("the kept text holds `b: u16`") as u32;
    let Mapped::Input { first, last } = map_range(view.column, at, at + 3) else { panic!("`u16` is an input token") };
    let input_at = entry.range.start + view.input_start;
    let written = &USER[input_at as usize..(input_at + view.input_len) as usize];
    let written_places = input_places(written, InputShape::Whole, Edition::E2021, alloc).expect("the call's input lexes");
    let (start, end) = (input_at + written_places[first as usize].0, input_at + written_places[last as usize].1);
    assert_eq!((start as usize, &USER[start as usize..end as usize]), (USER.find("u16").expect("the file holds `u16`"), "u16"), "`u16` maps to its own bytes");
}
