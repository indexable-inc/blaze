//! Piece sig-assoc-per-call, part L (rule 21): the read row of the impl text a member's
//! signature is read with is keyed by the IMPL, so it is re-checked through the impl's want
//! (blaze-cli body_types/heads.rs `want_of_fact`: a head fact is re-checked by its key's
//! `Signature` out, whose `unencoded` for an impl is its header and every non-fn member's text,
//! blaze-cli signature.rs). Keyed by the member fn, an edit of the impl's `type Out = ..;` in
//! another file moved no read of a calling body: a stale verdict.
//!
//! Fails without the fix: the row's `fact.key` is the fn's (`FN_A`), not the impl's.
//! Prints `a_member_s_impl_text_row_is_keyed_by_its_impl: key = impl, one fact for two members,
//! a type edit moves the answer`.

use blaze_own::AVec;
use blaze_typeck as tc;

use super::reads::FactKind;
use super::sig_impl::{DETAIL, ImplRead, impl_read};

const IMPL: tc::ItemKey = tc::ItemKey(0x1111);
const FN_A: u128 = 0x2222;
const FN_B: u128 = 0x3333;

fn member<'n, 'a>(assoc: &[(&'n str, &'n str)], alloc: blaze_own::Alloc<'a>) -> ImplRead<'n, 'a> {
    let mut list: AVec<'a, (&'n str, &'n str)> = AVec::new_in(alloc);
    list.extend_from_slice(assoc);
    ImplRead::Member { container: IMPL, text: "impl Tr for Foo { type Out = &'static S; fn a(&self) {} fn b(&self) {} }", self_ty: tc::PTy(0), assoc: list }
}

#[test]
fn a_member_s_impl_text_row_is_keyed_by_its_impl() {
    let alloc = blaze_own::testing::heap().alloc();
    let a = impl_read(FN_A, &member(&[("Out", "type Out = &'static S;")], alloc), alloc);
    assert_eq!((a.fact.kind, a.fact.key, a.fact.detail), (FactKind::BorrowHead, IMPL.0, DETAIL), "keyed by the impl: {a:?}");
    // Two members of one impl: one fact, one answer (reads.rs refuses only differing answers).
    let b = impl_read(FN_B, &member(&[("Out", "type Out = &'static S;")], alloc), alloc);
    assert_eq!((a.fact, a.answer), (b.fact, b.answer), "two members of one impl are one row");
    // The `type` member edited: the same fact, another answer.
    let c = impl_read(FN_A, &member(&[("Out", "type Out = &'a S;")], alloc), alloc);
    assert_eq!(a.fact, c.fact);
    assert_ne!(a.answer, c.answer, "an edit of `type Out` moves the row");
    // A fn that is no impl's member: keyed by the fn.
    let free = impl_read(FN_A, &ImplRead::NotMember, alloc);
    assert_eq!(free.fact.key, FN_A, "{free:?}");
    std::println!("a_member_s_impl_text_row_is_keyed_by_its_impl: key = impl, one fact for two members, a type edit moves the answer");
}
