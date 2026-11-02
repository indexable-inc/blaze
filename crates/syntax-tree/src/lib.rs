//! blaze's lossless syntax trees: flat trees ([`FlatTree`]) and the cursors over them.
#![forbid(
    // missing_debug_implementations,
    unconditional_recursion,
    future_incompatible,
    // missing_docs,
)]
#![deny(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[allow(unsafe_code)]
pub mod cursor;

pub mod api;
mod flat;
mod syntax_text;
mod utility_types;

pub mod ast;
pub mod view;
#[cfg(feature = "serde1")]
mod serde_impls;

pub use blaze_bytes::Text;
pub use text_size::{TextLen, TextRange, TextSize};

pub use crate::{
    api::{
        Language, SyntaxElement, SyntaxElementChildren, SyntaxNode, SyntaxNodeChildren,
        SyntaxNodeChildrenWhere, SyntaxToken,
    },
    flat::{Expand, FlatTree, FlatTreeBuilder, ImageError, Splice, IMAGE_LAYOUT},
    syntax_text::SyntaxText,
    utility_types::{Direction, NodeOrToken, TokenAtOffset, WalkEvent},
};

/// SyntaxKind is a type tag for each token or node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxKind(pub u16);
