//! A derived item's calls (names-attr-4, attr-bang-kind-2): its derives, and every one-segment
//! attribute on it, its variants and its fields that is neither the toolchain's nor a tool's, so
//! the cells' helper scope (`Cells::derive_helper`, names-attr-3) answers it.
//!
//! The Reference, "Procedural Macros", "Derive macro helper attributes": "Derive macros can add
//! additional attributes into the scope of the item they are on. Said attributes are called
//! derive macro helper attributes. These attributes are inert, and their only purpose is to be
//! fed into the derive macro that defined them." A built-in derive declares them the same way in
//! the pinned library (core/src/default.rs `#[rustc_builtin_macro(Default,
//! attributes(default))]`), which the lowering keeps (`MacroBody::BuiltinDerive`).
//!
//! On the item itself such a name is either a helper or an attribute macro that takes the item:
//! rustc takes the derive first, then each later attribute with the derive's helpers innermost
//! (`InvocationCollector::take_first_attr`). Which one is the cells' answer, so the item is sent
//! as calls ([`CallRole`]) and lowered by its keep call when no name took it.

use blaze_own::AVec;
use blaze_surface::{AttrKind, CallRole, CallShape, Item, ItemDetail, ItemIndex, MacroCallDecl};

use super::{AttrOrder, Attrs, CallSrc, Lower, LowerNoteKind, Walked, parse_path, unraw};

/// The path of a keep call: never resolved (the cells answer it from its item's other calls,
/// `Cells::item_fate`), only written, so it names what the call stands for.
const KEEP_PATH: &str = "derive";

impl<'t, 'a: 't, W: Walked + ?Sized> Lower<'_, 't, 'a, W> {
    /// The derive calls `derives` of item `idx` (written at `at`), in order; `input` is the
    /// item's text (blaze-expand's `ItemAttrs::derives` takes the derives out of it).
    pub(super) fn derive_calls(&mut self, idx: ItemIndex, item: &Item, at: u32, derives: &[&'t str], input: &'t str) {
        let place = self.call_in_text(idx, item, input);
        let tokens = self.call_tokens(input, crate::nested_input::InputCut::Whole);
        // The item the derives are on: the self type of the impls they write (`CallSrc::on`).
        let text = self.text;
        let on = item.name.map(|r| unraw(item.text(text, r)));
        for &path in derives {
            let call = self.key(self.owner, "derive", path);
            let Some(parsed) = parse_path(path, self.krate, self.alloc) else {
                self.note(LowerNoteKind::DerivePathNotRead, at, path);
                continue;
            };
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Derive, pos, item: Some(idx.0), role: CallRole::Call });
            let src = CallSrc { module: self.module, owner: None, name: path, input, kind: CallShape::Derive, loc: self.loc(at), written: self.written, depth: self.depth, implicit: false, place, tokens, answered: 0, on };
            self.out.calls.push((call, src));
        }
    }

    /// Sends the helper checks of kept item `item` (at `idx`), whose attributes are `attrs`, after
    /// its derive calls: one per name written on its variants and fields, at the first place it
    /// is written, on the item's index, so the cells read the derives before it on the same item
    /// (and, walking up, those an earlier stage's fragment sent: `Cells::derive_helper`). Only
    /// when this fragment sends derives (`attrs.derives[run..]`): a stage that sends none sent
    /// the checks itself ([`Lower::after_derive_takers`]), and on an item with no derive a
    /// one-segment attribute that is no built-in takes the item (`Cfg::attr`), and a variant's or
    /// a field's is not sent (a stated limit: such an attribute with no derive on its item is not
    /// checked). A name written on the item itself is answered there, never again.
    pub(super) fn helper_checks(&mut self, idx: ItemIndex, item: &Item, attrs: &Attrs<'t, 'a>, run: usize) {
        if attrs.derives.len() <= run || !matches!(item.detail, ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) {
            return;
        }
        // (name, the offset of the entry it is first written on): few per item, so a list.
        let mut uses: AVec<'a, (&'t str, u32)> = AVec::new_in(self.alloc);
        self.member_uses(idx, item, &attrs.others, &mut uses);
        self.send_checks(idx, &uses);
    }

    /// One stage of item `item` (at `idx`, a struct, an enum or a union) that has a derive and,
    /// written after it, attributes `attrs.others` that are not built in (or, in an expansion, a
    /// one-segment name that may be the helper of a derive on the item the expansion came from,
    /// read by the cells' `derive_helper`), the first `answered` of them answered inert by the
    /// fragments before this one. rustc's `take_first_attr` takes the attributes in written
    /// order: a derive runs on the item as it stands when it is reached; a helper is inert and
    /// the next is taken; an attribute macro takes the item, every attribute written after it
    /// (a later `#[derive]` among them, unexpanded) its input. So a stage is (attr-order-1):
    /// - the derive calls written after the names answered and before the stage's first name
    ///   (they run whatever the names are, on the item as written);
    /// - one `AfterDerive` call per name written before the next `#[derive]`, at its first
    ///   place: the whole item its input (blaze-expand's `ItemAttrs::attr_call` cuts the
    ///   attribute and the derives written before it, as rustc's macro receives it); the cells
    ///   decide each from the ones before it (`Cells::item_fate`);
    /// - one `Keep` call, input the whole item: when no name took the item its expansion is the
    ///   item ([`super::Fragment::kept`]), lowered from the next stage on; a later `#[derive]` is
    ///   sent there, never here, so it runs once: on the item kept, or in the output of the
    ///   macro that took it;
    /// - when no `#[derive]` is written after the stage, the checks of the names on its variants
    ///   and fields (inert when a name took the item: that macro's input); otherwise the last
    ///   stage's fragment sends them, beside the derives whose helpers they may be.
    ///
    /// The item's own text is not lowered here, as for any item an attribute macro may take.
    /// Under [`AttrOrder::DerivesAtOnce`] (the planted bug) the first stage sends every derive
    /// and every name.
    pub(super) fn after_derive_takers(&mut self, idx: ItemIndex, item: &Item, attrs: &Attrs<'t, 'a>, whole: &'t str, answered: usize) {
        let at = item.range.start;
        let at_once = self.order == AttrOrder::DerivesAtOnce;
        let run = ran_before(attrs, answered);
        // The derives this stage sends: up to the stage's first name.
        let upto = if at_once { attrs.derives.len() } else { attrs.derives_before[answered] };
        // The stage's names: from the first unanswered one to the next `#[derive]`.
        let mut end = answered;
        while end < attrs.others.len() && (at_once || attrs.derives_before[end] == upto) {
            end += 1;
        }
        self.derive_calls(idx, item, at, &attrs.derives[run..upto], whole);
        let place = self.call_in_text(idx, item, whole);
        let tokens = self.call_tokens(whole, crate::nested_input::InputCut::Whole);
        let mut takers: AVec<'a, &'t str> = AVec::new_in(self.alloc);
        for (&name, &derives_before) in attrs.others[answered..end].iter().zip(attrs.derives_before[answered..end].iter()) {
            if takers.contains(&name) {
                continue;
            }
            takers.push(name);
            let Some(parsed) = parse_path(name, self.krate, self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: name }, at, name);
                continue;
            };
            // A derive this fragment sends written after the name's first place: only under the
            // planted order, where that derive would run here and on the macro's output; the
            // cells refuse it then (`Unresolved::DeriveAfterAttrMacro`).
            let role = if derives_before < upto { CallRole::AfterDeriveBeforeDerive } else { CallRole::AfterDerive };
            let call = self.key(self.owner, "attr after derive", name);
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0), role });
            let src = CallSrc { module: self.module, owner: None, name, input: whole, kind: CallShape::Attr, loc: self.loc(at), written: self.written, depth: self.depth, implicit: false, place, tokens, answered: 0, on: None };
            self.out.calls.push((call, src));
        }
        let Some(parsed) = parse_path(KEEP_PATH, self.krate, self.alloc) else {
            self.note(LowerNoteKind::AttrMacroNotLowered { attr: KEEP_PATH }, at, KEEP_PATH);
            return;
        };
        let call = self.key(self.owner, "kept after derive", KEEP_PATH);
        let pos = self.next_pos();
        self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0), role: CallRole::Keep });
        let answered_here = u32::try_from(end).expect("an item's attributes are counted in u32");
        let src = CallSrc { module: self.module, owner: None, name: KEEP_PATH, input: whole, kind: CallShape::Attr, loc: self.loc(at), written: self.written, depth: self.depth, implicit: false, place, tokens, answered: answered_here, on: None };
        self.out.calls.push((call, src));
        if upto == attrs.derives.len() {
            let mut uses: AVec<'a, (&'t str, u32)> = AVec::new_in(self.alloc);
            self.member_uses(idx, item, &attrs.others, &mut uses);
            self.send_checks(idx, &uses);
        }
    }

    /// The one-segment names on `item`'s variants and fields (the entries after `idx` up to its
    /// end) that are neither built in nor a tool's and not in `sent`, each once at its first
    /// place. A variant or a field a false `cfg` removes is passed over with what it holds (rustc
    /// strips it before the derive sees the item).
    fn member_uses(&mut self, idx: ItemIndex, item: &Item, sent: &[&'t str], uses: &mut AVec<'a, (&'t str, u32)>) {
        let outline = self.outline;
        let end = item.end.at();
        let mut at = idx.at() + 1;
        // A work list over the item's entries in order: a skipped entry is skipped with the
        // entries written inside it (`end`), no recursion.
        while at < end {
            let index = ItemIndex(u32::try_from(at).expect("an outline's entries are counted in u32"));
            let member = outline.item(index);
            let past = member.end.at().max(at + 1);
            if !matches!(member.detail, ItemDetail::Variant { .. } | ItemDetail::Field { .. }) {
                at = past;
                continue;
            }
            if !outline.attrs(member).iter().any(|attr| attr.kind != AttrKind::Doc) {
                at += 1;
                continue;
            }
            let read = self.attrs(member);
            if !read.cfg_ok {
                at = past;
                continue;
            }
            // One-segment names alone: a helper is one segment, and a path on a member is an
            // attribute macro in a field's place, not checked (the stated limit above).
            for &name in read.others.iter().filter(|name| !name.contains("::")) {
                if !sent.contains(&name) {
                    push_once(uses, name, member.range.start);
                }
            }
            at += 1;
        }
    }

    /// One attribute check per entry of `uses` on item `idx`.
    fn send_checks(&mut self, idx: ItemIndex, uses: &[(&'t str, u32)]) {
        for &(name, offset) in uses {
            let Some(parsed) = parse_path(name, self.krate, self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: name }, offset, name);
                continue;
            };
            let call = self.key(self.owner, "helper check", name);
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0), role: CallRole::Call });
            // No input: a check is never expanded (`Cells::called` refuses an attribute macro in
            // a helper's place); only its place is read, for an error's location.
            let src = CallSrc { module: self.module, owner: None, name, input: "", kind: CallShape::Attr, loc: self.loc(offset), written: self.written, depth: self.depth, implicit: false, place: None, tokens: crate::nested_input::CallTokens::Text, answered: 0, on: None };
            self.out.calls.push((call, src));
            // A check, not a call that can grow the textual scope (`Lower::attr_checks`' span).
            self.checks_sent += 1;
        }
    }
}

/// How many of the item's derives ran before its after-derive name `answered` (the first not
/// answered): those written before the last answered name (`Attrs::derives_before`), 0 when none
/// is answered (attr-order-1).
pub(super) fn ran_before(attrs: &Attrs<'_, '_>, answered: usize) -> usize {
    match answered.checked_sub(1) {
        Some(last) => attrs.derives_before[last],
        None => 0,
    }
}

/// `name` at `offset`, unless already listed (the first place is kept).
fn push_once<'t>(uses: &mut AVec<'_, (&'t str, u32)>, name: &'t str, offset: u32) {
    if !uses.iter().any(|&(listed, _)| listed == name) {
        uses.push((name, offset));
    }
}
