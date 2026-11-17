//! The name a finding prints for its item: the item's OWN name, read through the entry's own
//! accessor (`Item::checked_in_file`), never a range re-sliced against the file (piece
//! pm-flow-adt, AGENTS.md rule 54).
//!
//! The class: `blaze_surface::Item::name` is ENTRY-RELATIVE (file_items.rs: "The ranges it
//! holds, each relative to its entry's start"); body_types.rs sliced it against the whole
//! file (`text.get(r.start..r.end)`), so every body not at the file's first byte printed four
//! wrong bytes. pm-small (leg 2, tree c61a1c3861, port27/tempo-deps/pm-6.out) printed
//! "body `Stre`" for `pub fn same`: its entry starts at byte 30 (`#[proc_macro]`), `same` is
//! entry-relative 21..25, and file bytes 21..25 are the `Stre` of line 1's `TokenStream`.
//! Every place a finding names its item takes [`shown_name`]; nothing else in the command
//! reads `Item::name` against the file's text.

use blaze_surface::Item;

/// The name of `item` as written in `text` (the file the outline was made from): `<unnamed>`
/// for an entry with no name (an impl, a `use`, `const _`); a name whose range does not lie in
/// `text` (an outline of another text: a bug of the caller) is said as such, never cut.
pub(crate) fn shown_name<'t>(item: &Item, text: &'t str) -> &'t str {
    let Some(name) = item.name else { return "<unnamed>" };
    match item.checked_in_file(name).and_then(|r| r.get(text)) {
        Some(shown) => shown,
        None => "<a name outside the file's text: the outline is not this text's>",
    }
}

#[cfg(test)]
mod tests {
    use super::shown_name;
    use blaze_surface::{Edition, ItemKind};

    /// pm-small's `mac/src/lib.rs`, whole. The fn's name is `same`; the old re-slice gave `Stre`
    /// (checked here as the control: if `Item::name` were file-relative, the control assert
    /// would fail and this test with it, saying the premise moved). Passes as
    /// "test item_name::tests::a_finding_names_the_whole_fn ... ok".
    #[test]
    fn a_finding_names_the_whole_fn() {
        let text = "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn same(input: TokenStream) -> TokenStream {\n    input\n}\n";
        let outline = blaze_file_items::outline(text, Edition::E2021, blaze_own::testing::heap().alloc());
        let item = outline.items().iter().find(|i| matches!(i.kind(), ItemKind::Function)).expect("the outline lists `fn same`");
        assert_eq!(shown_name(item, text), "same", "a finding names its fn by the whole name");
        let name = item.name.expect("a fn has a name");
        assert_eq!(text.get(name.start as usize..name.end as usize), Some("Stre"), "control: the entry-relative range sliced against the file is the old `Stre`");
    }
}
