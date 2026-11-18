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

/// What a range outside the file's text is said as (an outline of another text: a bug of the
/// caller), never cut.
const OUTSIDE: &str = "<a name outside the file's text: the outline is not this text's>";

/// The name of `item` as written in `text` (the file the outline was made from). An entry with
/// no name (an impl, a `use`, an extern block, a macro call, `const _`) is shown by its head as
/// written ([`shown_head`]: `impl Serialize for Point`, `make!`, `const _: ()`), never as a
/// placeholder; a name whose range does not lie in `text` is said as such ([`OUTSIDE`]).
pub(crate) fn shown_name<'t>(item: &Item, text: &'t str) -> &'t str {
    let Some(name) = item.name else { return shown_head(item, text) };
    match item.checked_in_file(name).and_then(|r| r.get(text)) {
        Some(shown) => shown,
        None => OUTSIDE,
    }
}

/// The head of `item` as written, borrowed from `text`: its `signature` range (to its body's
/// start) without its outer attributes and comments, up to the end of its first line, trailing
/// space trimmed (`impl<T> Tr for A<T>` of a head whose `where` is on the next line). Each step
/// moves forward in the head, so the scan is bounded by its length.
fn shown_head<'t>(item: &Item, text: &'t str) -> &'t str {
    let Some(head) = item.checked_in_file(item.signature).and_then(|r| r.get(text)) else { return OUTSIDE };
    let mut rest = head;
    for _ in 0..=head.len() {
        rest = rest.trim_start();
        let skipped = if rest.starts_with('#') {
            attribute_len(rest)
        } else if rest.starts_with("//") {
            rest.find('\n').unwrap_or(rest.len())
        } else if rest.starts_with("/*") {
            rest.find("*/").map_or(rest.len(), |at| at + 2)
        } else {
            0
        };
        if skipped == 0 {
            break;
        }
        rest = rest.get(skipped..).unwrap_or("");
    }
    let line = rest.split('\n').next().unwrap_or("").trim_end();
    if line.is_empty() { head.trim() } else { line }
}

/// The length of the attribute `#[..]` or `#![..]` that `at` starts with, to its closing `]`
/// at depth zero (brackets inside strings are not told apart: a shown head, not a parse);
/// `at`'s whole length when it does not close, 0 when `at` is no attribute.
fn attribute_len(at: &str) -> usize {
    let open = if at.starts_with("#![") { 2 } else if at.starts_with("#[") { 1 } else { return 0 };
    let mut depth = 0u32;
    for (offset, byte) in at.bytes().enumerate().skip(open) {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return offset + 1;
                }
            }
            _ => {}
        }
    }
    at.len()
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
