//! `blaze-html`: `#![no_std]` `#![blaze::sans_io]` `#![forbid(unsafe_code)]` HTML5 tokenizer,
//! flat arena-indexed DOM, and layout-ready visual document renderer.
//!
//! Designed for both standalone `.html` documents and embedded HTML inside Markdown (`README.md`,
//! `<picture>` dark/light theme selection, `<p align="center">`, `<h3 align="center">`, `<code>`,
//! `<kbd>`, `<br>`, `<a href="...">`, `<table>`, `<details>`, `<ul>`/`<ol>`, HTML comments &
//! character entities). Every allocation goes through the caller's [`blaze_own::Alloc`], and every
//! traversal uses an explicit worklist bounded by [`HtmlLimits`] (zero recursion, zero panics).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

pub mod dom;
pub mod entities;
pub mod render;
pub mod tokenizer;

#[cfg(test)]
mod tests;

pub use crate::dom::{
    Attr, ColorScheme, Document, HtmlLimits, Node, NodeId, NodeKind, PictureChoice, Tag,
    TextAlign, parse_html,
};
pub use crate::entities::decode_entities;
pub use crate::render::{
    InlineStyle, RenderBlock, RenderSpan, RenderTable, render_document, render_html,
};
pub use crate::tokenizer::{RawAttr, Token, Tokenizer};
