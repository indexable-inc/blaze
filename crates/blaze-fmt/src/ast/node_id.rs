//! Node ids, as `compiler/rustc_ast/src/node_id.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0) has them. Nothing is resolved here, so every node carries the dummy id.

use std::fmt;

use crate::span::LocalExpnId;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(u32);

pub const CRATE_NODE_ID: NodeId = NodeId(0);

pub const DUMMY_NODE_ID: NodeId = NodeId(u32::MAX);

impl NodeId {
    pub fn placeholder_from_expn_id(_expn_id: LocalExpnId) -> NodeId {
        DUMMY_NODE_ID
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
