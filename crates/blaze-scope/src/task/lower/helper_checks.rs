//! A derive's helper attributes reach the cells (names-attr-4): every one-segment attribute on a
//! derived item, its variants and its fields that is neither the toolchain's nor a tool's is sent
//! as an attribute check, so the cells' helper scope (`Cells::derive_helper`, names-attr-3)
//! answers it: inert when a derive written before it on the item declares the name, else it
//! resolves as any attribute and a name nothing binds is "cannot find attribute".
//!
//! The Reference, "Procedural Macros", "Derive macro helper attributes": "Derive macros can add
//! additional attributes into the scope of the item they are on. Said attributes are called
//! derive macro helper attributes. These attributes are inert, and their only purpose is to be
//! fed into the derive macro that defined them." A built-in derive declares them the same way in
//! the pinned library (core/src/default.rs `#[rustc_builtin_macro(Default,
//! attributes(default))]`), which the lowering keeps (`MacroBody::BuiltinDerive`).

use blaze_own::AVec;
use blaze_surface::{AttrKind, CallShape, Item, ItemDetail, ItemIndex, MacroCallDecl};

use super::{Attrs, CallSrc, Lower, LowerNoteKind, Walked, parse_path};

impl<'t, 'a: 't, W: Walked + ?Sized> Lower<'_, 't, 'a, W> {
    /// Sends the helper checks of kept item `item` (at `idx`), whose attributes are `attrs`, after
    /// its derive calls: one per name, at the first place it is written, on the item's index, so
    /// the cells read the derives before it on the same item. Only an item with a derive: on any
    /// other a one-segment attribute that is no built-in takes the item (`Cfg::attr`), and a
    /// variant's or a field's is not sent (a stated limit: such an attribute with no derive on its
    /// item is not checked). A variant or a field a false `cfg` removes is passed over with what
    /// it holds (rustc strips it before the derive sees the item).
    pub(super) fn helper_checks(&mut self, idx: ItemIndex, item: &Item, attrs: &Attrs<'t, 'a>) {
        if attrs.derives.is_empty() || !matches!(item.detail, ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) {
            return;
        }
        // (name, the offset of the entry it is first written on): few per item, so a list.
        let mut uses: AVec<'a, (&'t str, u32)> = AVec::new_in(self.alloc);
        // On a kept item every such name follows a derive: one before every derive would have
        // taken the item (`Cfg::attr`'s candidate test).
        for &name in attrs.others.iter() {
            push_once(&mut uses, name, item.range.start);
        }
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
            for &name in read.others.iter() {
                push_once(&mut uses, name, member.range.start);
            }
            at += 1;
        }
        for &(name, offset) in uses.iter() {
            let Some(parsed) = parse_path(name, self.krate, self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: name }, offset, name);
                continue;
            };
            let call = self.key(self.owner, "helper check", name);
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0) });
            // No input: a check is never expanded (`Cells::called` refuses an attribute macro in
            // a helper's place); only its place is read, for an error's location.
            let src = CallSrc { module: self.module, owner: None, name, input: "", kind: CallShape::Attr, loc: self.loc(offset), depth: self.depth, implicit: false };
            self.out.calls.push((call, src));
            // A check, not a call that can grow the textual scope (`Lower::attr_checks`' span).
            self.checks_sent += 1;
        }
    }
}

/// `name` at `offset`, unless already listed (the first place is kept).
fn push_once<'t>(uses: &mut AVec<'_, (&'t str, u32)>, name: &'t str, offset: u32) {
    if !uses.iter().any(|&(listed, _)| listed == name) {
        uses.push((name, offset));
    }
}
