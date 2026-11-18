//! Lane B12, item 6: a place row's reasons a move out of it is refused are two facts, never
//! one bit read two ways (flow.rs `PlaceRow::of_drop_type`, `PlaceRow::of_slice`). Each mark
//! sets its own fact and only it; an unmarked projection sets neither; a place is interned by
//! its parent and projection, so a second ask finds the first row and its mark. NEVER COMPILED
//! when written (2026-11-18, lane B12). Every allocation goes to one named buffer
//! ([`Inline`]).

use blaze_own::Inline;

use crate::flow::{FlowBuilder, PlaceMark, Proj, Span};

const BYTES: usize = 1 << 14;
const S: Span = Span { lo: 0, hi: 0 };

/// Prints `place_marks: ok` after the asserts.
#[test]
fn each_mark_is_its_own_fact() {
    let inline = Inline::<BYTES>::new();
    let mut b = FlowBuilder::new_in(2, 1, &inline);
    let root = b.local(1);
    let elem = b.project_in(root, Proj::Index, PlaceMark::SliceElement);
    let field = b.project_in(root, Proj::Field(0), PlaceMark::DropField);
    let plain = b.project_in(root, Proj::Field(1), PlaceMark::None);
    let again = b.project_in(root, Proj::Index, PlaceMark::None);
    let old = b.project_marked(root, Proj::Field(2), true);
    let bb = b.block();
    b.start(bb);
    b.ret(S);
    let f = b.finish().expect("a well-formed flow");
    crate::loans::assert_tiers_equal_for_fixture(&f);
    let (e, d, p, o) = (f.place(elem), f.place(field), f.place(plain), f.place(old));
    assert!(e.of_slice && !e.of_drop_type, "{e:?}");
    // CONTROL: the `Drop` field mark is not the slice fact (before item 6 one bit meant both).
    assert!(d.of_drop_type && !d.of_slice, "{d:?}");
    assert!(!p.of_drop_type && !p.of_slice, "{p:?}");
    assert!(o.of_drop_type && !o.of_slice, "`project_marked(.., true)` is `DropField`: {o:?}");
    assert_eq!(again, elem, "interned: the first row, its mark kept");
    std::println!("place_marks: ok");
}
