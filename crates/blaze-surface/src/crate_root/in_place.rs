//! A [`CrateRoot`](super) record read in place with no check (D114 rule 1; rule 74, verify
//! once): [`CrateRootView::trusted`] views bytes [`CrateRootView::read`] accepted before in
//! this process, in O(1) and with no allocation, and the two lookups the path walk makes of a
//! crate root that the view lacked.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): inert until `mod in_place;` is added to
//! crate_root.rs by the wiring.

use blaze_format::field;
use blaze_id::{CrateItemKey, ItemKey};

use super::{CrateRootHead, CrateRootRoot, CrateRootView};
use crate::column::{key_of_word, text_of};
use crate::kind::MacroShapes;

impl<'a> CrateRootView<'a> {
    /// The record in `buffer` with no check at all: O(1), no allocation.
    ///
    /// # Safety
    ///
    /// [`CrateRootView::read`] returned `Ok` for these same bytes, at this same address,
    /// earlier in this process, and they have not changed since. The one caller is
    /// blaze-scope's names record, whose `Verified` proof is made only by running `read` on the
    /// crate root it holds.
    pub unsafe fn trusted(buffer: &'a [u8]) -> CrateRootView<'a> {
        // SAFETY: `read` began with `blaze_format::open::<CrateRootRoot>(buffer)`, which
        // returned `Ok` for these unchanged bytes at this address (the caller's promise).
        let root = unsafe { blaze_format::reopen::<CrateRootRoot>(buffer) };
        let text: &'a [u8] = field!(root, CrateRootRoot, strings).items().as_slice();
        // SAFETY: `read` took the strings column with `Slice::as_str`, which refuses bytes that
        // are not UTF-8; the bytes are the same.
        let strings = unsafe { core::str::from_utf8_unchecked(text) };
        CrateRootView {
            head: CrateRootHead {
                krate: CrateItemKey(root.krate),
                root_module: ItemKey(root.root_module),
                root_file: root.root_file,
                edition: root.edition,
                no_std: root.no_std,
                no_core: root.no_core,
                arbitrary_ints: root.arbitrary_ints,
                prelude: key_of_word(root.prelude),
            },
            externs: field!(root, CrateRootRoot, externs).items().as_slice(),
            deps: field!(root, CrateRootRoot, deps).items().as_slice(),
            macro_use: field!(root, CrateRootRoot, macro_use).items().as_slice(),
            macro_exports: field!(root, CrateRootRoot, macro_exports).items().as_slice(),
            tools: field!(root, CrateRootRoot, tools).items().as_slice(),
            builtin_attrs: field!(root, CrateRootRoot, builtin_attrs).items().as_slice(),
            features: field!(root, CrateRootRoot, features).items().as_slice(),
            strings,
        }
    }

    /// The `i`th crate of the `#[macro_use]` prelude, by its root module.
    pub fn macro_use_at(&self, i: usize) -> Option<ItemKey> {
        self.macro_use.get(i).map(|key| ItemKey(*key))
    }

    /// The `#[macro_export]` macro written in source named `name`, the one of least key when
    /// two are, by one search of the rows (sorted by name, then key).
    pub fn macro_export(&self, name: &str) -> Option<(ItemKey, MacroShapes)> {
        let strings = self.strings;
        let at = self.macro_exports.partition_point(|row| text_of(strings, row.name) < name);
        let row = self.macro_exports.get(at).filter(|row| text_of(strings, row.name) == name)?;
        Some((ItemKey(row.key), row.shapes))
    }
}
