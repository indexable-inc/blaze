//! An item's chain (`blaze_file_items::item_chain`): one identity per entry of a text, which no
//! edit elsewhere in the text moves.
//!
//! Expected line: `CHAIN: 9 entries, 9 chains; an edit above moved none; the impl's part
//! removed: 2 collide`.

#![feature(allocator_api)]

use blaze_file_items::{CHAIN_DOMAIN, Outline, chain_at, head_of, item_chain, outline};
use blaze_ids::{ContentId, Hasher};
use blaze_surface::{Edition, Item, ItemIndex, ItemKind};

/// Two impls with a member of one name, two `use` items (no name: told apart by their heads),
/// and a free fn of the members' name.
const TEXT: &str = "use a::b;
use a::c;
struct A;
struct B;
impl A {
    fn get(&self) -> u8 {
        1
    }
}
impl B {
    fn get(&self) -> u8 {
        2
    }
}
fn get() -> u8 {
    3
}
";

/// The planted bug: the chain of a member computed from its own entry alone, without the entry
/// it is written in (its impl's part).
fn chain_without_parents(item: &Item, text: &str) -> ContentId {
    let mut h = Hasher::new(CHAIN_DOMAIN);
    let name = item.name.map_or("", |range| item.text(text, range));
    h.update(&[item.kind().byte()]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&item.disambiguator.to_le_bytes());
    if item.name.is_none() {
        h.update(&head_of(item).0);
    }
    h.finish()
}

fn chains(outline: &Outline<'_>, text: &str) -> Vec<(ItemKind, String, ContentId)> {
    outline.items().iter().map(|item| (item.kind(), item.name.map_or("", |range| item.text(text, range)).to_string(), item_chain(outline, item, text))).collect()
}

#[test]
fn every_entry_has_its_own_chain_and_an_edit_elsewhere_moves_none() {
    let made = outline(TEXT, Edition::E2024, blaze_own::testing::heap().alloc());
    let now = chains(&made, TEXT);
    // Two `use`, two structs, two impls, their two members and the free fn.
    assert_eq!(now.len(), 9, "the entries of the text: {now:?}");
    for (i, a) in now.iter().enumerate() {
        assert_eq!(a.2, chain_at(&made, ItemIndex(i as u32), TEXT), "entry {i}: by index and by reference");
        for (j, b) in now.iter().enumerate().skip(i + 1) {
            assert_ne!(a.2, b.2, "entries {i} ({:?} `{}`) and {j} ({:?} `{}`) share a chain", a.0, a.1, b.0, b.1);
        }
    }

    // An edit elsewhere: a comment above everything and another body for the free fn. Every
    // entry keeps its chain (a chain holds no position and no body).
    let edited = format!("// a line above\n\n{}", TEXT.replace("    3\n", "    30 + 3\n"));
    let made_edited = outline(&edited, Edition::E2024, blaze_own::testing::heap().alloc());
    let later = chains(&made_edited, &edited);
    assert_eq!(now, later, "an edit to a comment and to one body moved a chain");

    // The planted bug: without the impl's part the two `get` members and the free `get` are
    // told apart by nothing.
    let fns: Vec<&Item> = made.items().iter().filter(|item| item.kind() == ItemKind::Function).collect();
    assert_eq!(fns.len(), 3, "three fns named `get`");
    let members: Vec<&&Item> = fns.iter().filter(|item| item.parent.is_some()).collect();
    assert_eq!(members.len(), 2, "two of them members");
    assert_eq!(chain_without_parents(members[0], TEXT), chain_without_parents(members[1], TEXT), "the planted bug (a chain without its impl's part) did not make the two members collide");
    assert_ne!(item_chain(&made, members[0], TEXT), item_chain(&made, members[1], TEXT), "the chain tells the two members apart");
    println!("CHAIN: {} entries, {} chains; an edit above moved none; the impl's part removed: 2 collide", now.len(), now.len());
}
