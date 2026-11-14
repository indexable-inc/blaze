//! The four record kinds of lint results, declared in the core's one registry
//! (`blaze_keyed::kind`, listed in its `TAGS`) with their key rules, and re-exported here.
//!
//! | kind | tag | key | trace | root |
//! |---|---|---|---|---|
//! | [`BodyFindings`] | 16 | tool, body key, pass, other items' summaries it reads | reuse | traced |
//! | [`ItemRows`] | 17 | the same | reuse | traced |
//! | [`FoldFindings`] | 18 | tool, pass, scope, leaf, digest of what the leaf folded | key | opaque |
//! | [`DeferredFindings`] | 19 | tool, pass, item, digests of everything it read | key | opaque |
//!
//! No key covers a level, a group or the ratchet: those are the view's. A body findings record
//! is the body check's sibling: its key is the body's key (which covers the body's text and the
//! closure of every name it mentions) and the keys of the summaries of other items the pass
//! reads (a callee's effect summary, known from the body's record before the pass runs), so an
//! equal key is the finding with no other check; a key miss with an unchanged read list files
//! the old record under the new key, byte for byte (`Trace::Reuse`).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

pub use blaze_keyed::kind::{
    BodyFindings, DeferredFindings, DeferredInputs, FoldFindings, FoldInputs, ItemRows, Pass, PerBody, ROOT_LEAF,
};
use blaze_keyed::kind::Kind;

use crate::finding::FORMAT;
use crate::pass::PassRef;

// Every findings kind's version is this crate's payload format: a format bump without a bump
// of the kinds' versions in the core's registry does not compile.
const _: () = assert!(
    BodyFindings::VERSION as u32 == FORMAT
        && ItemRows::VERSION as u32 == FORMAT
        && FoldFindings::VERSION as u32 == FORMAT
        && DeferredFindings::VERSION as u32 == FORMAT,
    "the findings kinds' versions in blaze_keyed::kind are not blaze-findings' payload FORMAT"
);

impl PassRef {
    /// The pass as a findings key names it.
    pub fn keyed(self) -> Pass {
        Pass { id: self.id.0, version: self.version }
    }
}
