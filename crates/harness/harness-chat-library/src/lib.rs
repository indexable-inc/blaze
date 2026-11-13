//! Local conversations, discovered without starting or taking over their agents.
//! Parsing is bytes in, chat events out; filesystem access is confined to `source`'s drivers.

#![feature(custom_inner_attributes, register_tool)]
#![register_tool(blaze)]

mod index;
mod metadata;
mod source;

pub use index::{Hit, Index};
pub use metadata::{SessionMetadata, apply_metadata, metadata};
pub use source::{Parsed, Roots, Session, Source, discover, parse, read, read_session};
