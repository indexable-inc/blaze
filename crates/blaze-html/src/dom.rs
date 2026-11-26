//! Flat arena-indexed HTML5 DOM tree (`Document<'a>`) with explicit bounds, void-element
//! handling, implicit tag closing, attribute queries, alignment extraction, and dark/light
//! `<picture>` source selection.

use blaze_own::{AString, AVec, Alloc};

use crate::entities::decode_entities;
use crate::tokenizer::{Token, Tokenizer};

/// Newtype index into [`Document::nodes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const ROOT: Self = Self(0);

    #[inline]
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// Color scheme for `<picture><source media="(prefers-color-scheme: ...)">` resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

/// Horizontal text / media alignment extracted from `align="..."`, `<center>`, or inline CSS
/// `style="text-align: ..."`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Hard bounds guaranteeing deterministic O(N) parsing with bounded memory (rule 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtmlLimits {
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_attrs_per_tag: usize,
}

impl HtmlLimits {
    pub const SHIPPED: Self = Self {
        max_nodes: 16_384,
        max_depth: 128,
        max_attrs_per_tag: 32,
    };
}

/// Semantic HTML5 tag classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    // Document & metadata (non-visual)
    Html,
    Head,
    Title,
    Meta,
    Link,
    Style,
    Script,
    Noscript,
    Template,
    // Block containers
    Body,
    Main,
    Article,
    Section,
    Header,
    Footer,
    Nav,
    Aside,
    Div,
    P,
    Center,
    Figure,
    Figcaption,
    Details,
    Summary,
    Blockquote,
    Pre,
    Hr,
    Br,
    // Headings
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
    // Lists
    Ul,
    Ol,
    Li,
    Dl,
    Dt,
    Dd,
    // Tables
    Table,
    Thead,
    Tbody,
    Tfoot,
    Tr,
    Th,
    Td,
    Caption,
    // Media
    Picture,
    Source,
    Img,
    Svg,
    // Inline
    A,
    Strong,
    B,
    Em,
    I,
    Code,
    Kbd,
    Samp,
    Var,
    S,
    Del,
    U,
    Ins,
    Mark,
    Small,
    Sub,
    Sup,
    Span,
    Abbr,
    Time,
    Q,
    // Void & other
    Area,
    Base,
    Col,
    Embed,
    Input,
    Param,
    Track,
    Wbr,
    Unknown,
}

impl Tag {
    /// Classify an HTML tag name (ASCII case-insensitive).
    pub fn from_name(raw: &str) -> Self {
        let mut buf = [0u8; 16];
        let bytes = raw.as_bytes();
        if bytes.len() > buf.len() {
            return Self::Unknown;
        }
        for (dst, &src) in buf.iter_mut().zip(bytes.iter()) {
            *dst = src.to_ascii_lowercase();
        }
        let lower = buf
            .get(..bytes.len())
            .and_then(|s| core::str::from_utf8(s).ok())
            .unwrap_or("");
        match lower {
            "html" => Self::Html,
            "head" => Self::Head,
            "title" => Self::Title,
            "meta" => Self::Meta,
            "link" => Self::Link,
            "style" => Self::Style,
            "script" => Self::Script,
            "noscript" => Self::Noscript,
            "template" => Self::Template,
            "body" => Self::Body,
            "main" => Self::Main,
            "article" => Self::Article,
            "section" => Self::Section,
            "header" => Self::Header,
            "footer" => Self::Footer,
            "nav" => Self::Nav,
            "aside" => Self::Aside,
            "div" => Self::Div,
            "p" => Self::P,
            "center" => Self::Center,
            "figure" => Self::Figure,
            "figcaption" => Self::Figcaption,
            "details" => Self::Details,
            "summary" => Self::Summary,
            "blockquote" => Self::Blockquote,
            "pre" => Self::Pre,
            "hr" => Self::Hr,
            "br" => Self::Br,
            "h1" => Self::H1,
            "h2" => Self::H2,
            "h3" => Self::H3,
            "h4" => Self::H4,
            "h5" => Self::H5,
            "h6" => Self::H6,
            "ul" => Self::Ul,
            "ol" => Self::Ol,
            "li" => Self::Li,
            "dl" => Self::Dl,
            "dt" => Self::Dt,
            "dd" => Self::Dd,
            "table" => Self::Table,
            "thead" => Self::Thead,
            "tbody" => Self::Tbody,
            "tfoot" => Self::Tfoot,
            "tr" => Self::Tr,
            "th" => Self::Th,
            "td" => Self::Td,
            "caption" => Self::Caption,
            "picture" => Self::Picture,
            "source" => Self::Source,
            "img" => Self::Img,
            "svg" => Self::Svg,
            "a" => Self::A,
            "strong" => Self::Strong,
            "b" => Self::B,
            "em" => Self::Em,
            "i" => Self::I,
            "code" => Self::Code,
            "kbd" => Self::Kbd,
            "samp" => Self::Samp,
            "var" => Self::Var,
            "s" | "strike" => Self::S,
            "del" => Self::Del,
            "u" => Self::U,
            "ins" => Self::Ins,
            "mark" => Self::Mark,
            "small" => Self::Small,
            "sub" => Self::Sub,
            "sup" => Self::Sup,
            "span" => Self::Span,
            "abbr" => Self::Abbr,
            "time" => Self::Time,
            "q" => Self::Q,
            "area" => Self::Area,
            "base" => Self::Base,
            "col" => Self::Col,
            "embed" => Self::Embed,
            "input" => Self::Input,
            "param" => Self::Param,
            "track" => Self::Track,
            "wbr" => Self::Wbr,
            _ => Self::Unknown,
        }
    }

    /// True for HTML5 void elements that never have children or a closing tag.
    pub const fn is_void(self) -> bool {
        matches!(
            self,
            Self::Area
                | Self::Base
                | Self::Br
                | Self::Col
                | Self::Embed
                | Self::Hr
                | Self::Img
                | Self::Input
                | Self::Link
                | Self::Meta
                | Self::Param
                | Self::Source
                | Self::Track
                | Self::Wbr
        )
    }

    /// True for non-visual elements whose subtrees are skipped during document rendering.
    pub const fn is_non_visual(self) -> bool {
        matches!(
            self,
            Self::Head | Self::Style | Self::Script | Self::Template | Self::Title | Self::Meta | Self::Link
        )
    }

    /// True for block-level tags that implicitly close an open `<p>`.
    pub const fn closes_p(self) -> bool {
        matches!(
            self,
            Self::Article
                | Self::Aside
                | Self::Blockquote
                | Self::Center
                | Self::Details
                | Self::Div
                | Self::Dl
                | Self::Figure
                | Self::Footer
                | Self::H1
                | Self::H2
                | Self::H3
                | Self::H4
                | Self::H5
                | Self::H6
                | Self::Header
                | Self::Hr
                | Self::Main
                | Self::Nav
                | Self::Ol
                | Self::P
                | Self::Pre
                | Self::Section
                | Self::Table
                | Self::Ul
        )
    }

    /// Heading level `1..=6`, if this tag is `<h1>`..`<h6>`.
    pub const fn heading_level(self) -> Option<u8> {
        match self {
            Self::H1 => Some(1),
            Self::H2 => Some(2),
            Self::H3 => Some(3),
            Self::H4 => Some(4),
            Self::H5 => Some(5),
            Self::H6 => Some(6),
            _ => None,
        }
    }
}

/// Decoded HTML attribute stored in [`Document::attrs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr<'a> {
    pub name: AString<'a>,
    pub value: AString<'a>,
}

/// Node payload inside [`Document::nodes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind<'a> {
    Document,
    Element {
        tag: Tag,
        tag_name: AString<'a>,
        attr_start: u32,
        attr_len: u32,
    },
    Text(AString<'a>),
    Comment,
}

/// One node in the flat sibling-linked DOM arena.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub kind: NodeKind<'a>,
}

/// Resolved media target from `<picture>` or `<img>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PictureChoice<'a> {
    pub src: AString<'a>,
    pub alt: AString<'a>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Arena-allocated HTML5 DOM tree.
#[derive(Clone, Debug)]
pub struct Document<'a> {
    pub nodes: AVec<'a, Node<'a>>,
    pub attrs: AVec<'a, Attr<'a>>,
    pub truncated: bool,
}

impl<'a> Document<'a> {
    /// Look up a node by [`NodeId`].
    #[inline]
    pub fn node(&self, id: NodeId) -> Option<&Node<'a>> {
        self.nodes.get(id.index())
    }

    /// Borrow all attributes of element `id`.
    pub fn element_attrs(&self, id: NodeId) -> &[Attr<'a>] {
        let Some(node) = self.node(id) else {
            return &[];
        };
        let NodeKind::Element { attr_start, attr_len, .. } = node.kind else {
            return &[];
        };
        let start = usize::try_from(attr_start).unwrap_or(0);
        let len = usize::try_from(attr_len).unwrap_or(0);
        let end = start.saturating_add(len);
        self.attrs.get(start..end).unwrap_or(&[])
    }

    /// Find attribute value by lowercase name on element `id`.
    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        for a in self.element_attrs(id) {
            if a.name.as_str().eq_ignore_ascii_case(name) {
                return Some(a.value.as_str());
            }
        }
        None
    }

    /// Extract explicit text alignment from `align="..."`, `<center>`, or `style="text-align: ..."`.
    pub fn element_align(&self, id: NodeId) -> Option<TextAlign> {
        if let Some(node) = self.node(id)
            && matches!(node.kind, NodeKind::Element { tag: Tag::Center, .. })
        {
            return Some(TextAlign::Center);
        }
        if let Some(a) = self.attr(id, "align") {
            let trimmed = a.trim();
            if trimmed.eq_ignore_ascii_case("center") {
                return Some(TextAlign::Center);
            }
            if trimmed.eq_ignore_ascii_case("right") {
                return Some(TextAlign::Right);
            }
            if trimmed.eq_ignore_ascii_case("left") {
                return Some(TextAlign::Left);
            }
        }
        if let Some(style) = self.attr(id, "style") {
            for decl in style.split(';') {
                if let Some((k, v)) = decl.split_once(':')
                    && k.trim().eq_ignore_ascii_case("text-align")
                {
                    let val = v.trim();
                    if val.eq_ignore_ascii_case("center") {
                        return Some(TextAlign::Center);
                    }
                    if val.eq_ignore_ascii_case("right") {
                        return Some(TextAlign::Right);
                    }
                    if val.eq_ignore_ascii_case("left") {
                        return Some(TextAlign::Left);
                    }
                }
            }
        }
        None
    }

    /// Resolve a `<picture>` element for `scheme`, selecting a matching `<source media="..." srcset="...">`
    /// over the fallback `<img>` child while preserving `alt`, `width`, and `height`.
    pub fn resolve_picture(&self, picture_id: NodeId, scheme: ColorScheme, alloc: Alloc<'a>) -> Option<PictureChoice<'a>> {
        let mut matched_src: Option<&str> = None;
        let mut fallback_img: Option<NodeId> = None;

        let mut cur = self.node(picture_id).and_then(|n| n.first_child);
        while let Some(cid) = cur {
            let Some(child) = self.node(cid) else {
                break;
            };
            if let NodeKind::Element { tag, .. } = child.kind {
                match tag {
                    Tag::Source if matched_src.is_none() => {
                        let media = self.attr(cid, "media").unwrap_or("");
                        if media_matches_scheme(media, scheme)
                            && let Some(srcset) = self.attr(cid, "srcset")
                            && let Some(url) = first_srcset_url(srcset)
                        {
                            matched_src = Some(url);
                        }
                    }
                    Tag::Img if fallback_img.is_none() => {
                        fallback_img = Some(cid);
                    }
                    _ => {}
                }
            }
            cur = child.next_sibling;
        }

        let img_id = fallback_img;
        let src = matched_src
            .or_else(|| img_id.and_then(|id| self.attr(id, "src")))
            .unwrap_or("");
        if src.is_empty() {
            return None;
        }
        let alt = img_id.and_then(|id| self.attr(id, "alt")).unwrap_or("");
        let width = img_id.and_then(|id| self.attr(id, "width")).and_then(parse_px_dim);
        let height = img_id.and_then(|id| self.attr(id, "height")).and_then(parse_px_dim);
        Some(PictureChoice {
            src: AString::from_str_in(src, alloc),
            alt: AString::from_str_in(alt, alloc),
            width,
            height,
        })
    }

    /// Resolve a standalone `<img>` element into [`PictureChoice`].
    pub fn resolve_img(&self, img_id: NodeId, alloc: Alloc<'a>) -> Option<PictureChoice<'a>> {
        let src = self.attr(img_id, "src").unwrap_or("");
        let alt = self.attr(img_id, "alt").unwrap_or("");
        if src.is_empty() && alt.is_empty() {
            return None;
        }
        Some(PictureChoice {
            src: AString::from_str_in(src, alloc),
            alt: AString::from_str_in(alt, alloc),
            width: self.attr(img_id, "width").and_then(parse_px_dim),
            height: self.attr(img_id, "height").and_then(parse_px_dim),
        })
    }

    fn append_node(&mut self, parent: NodeId, kind: NodeKind<'a>) -> Option<NodeId> {
        let next_idx = u32::try_from(self.nodes.len()).ok()?;
        let new_id = NodeId(next_idx);
        self.nodes.push(Node {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            next_sibling: None,
            kind,
        });
        let prev_last = self.nodes.get(parent.index()).and_then(|p| p.last_child);
        if let Some(prev_id) = prev_last {
            if let Some(prev) = self.nodes.get_mut(prev_id.index()) {
                prev.next_sibling = Some(new_id);
            }
            if let Some(p) = self.nodes.get_mut(parent.index()) {
                p.last_child = Some(new_id);
            }
        } else if let Some(p) = self.nodes.get_mut(parent.index()) {
            p.first_child = Some(new_id);
            p.last_child = Some(new_id);
        }
        Some(new_id)
    }
}

/// Parse an HTML5 document or fragment into an arena [`Document<'a>`] using `alloc`.
pub fn parse_html<'a>(src: &str, limits: HtmlLimits, alloc: Alloc<'a>) -> Document<'a> {
    let mut nodes = AVec::new_in(alloc);
    nodes.push(Node {
        parent: None,
        first_child: None,
        last_child: None,
        next_sibling: None,
        kind: NodeKind::Document,
    });
    let mut doc = Document { nodes, attrs: AVec::new_in(alloc), truncated: false };
    let mut open: AVec<'a, (NodeId, Tag)> = AVec::new_in(alloc);
    open.push((NodeId::ROOT, Tag::Html));

    let mut tok = Tokenizer::new(src);
    while let Some(token) = tok.next_token(limits.max_attrs_per_tag, alloc) {
        if doc.nodes.len() >= limits.max_nodes {
            doc.truncated = true;
            break;
        }
        match token {
            Token::Doctype(_) | Token::Comment(_) => {}
            Token::Text(raw) => {
                if raw.is_empty() {
                    continue;
                }
                let decoded = decode_entities(raw, alloc);
                let parent = open.last().map_or(NodeId::ROOT, |&(id, _)| id);
                let _ = doc.append_node(parent, NodeKind::Text(decoded));
            }
            Token::StartTag { name, attrs, self_closing } => {
                let tag = Tag::from_name(name);
                apply_implicit_closes(&mut open, tag);

                let attr_start = u32::try_from(doc.attrs.len()).unwrap_or(u32::MAX);
                let mut attr_count: u32 = 0;
                for raw_attr in attrs.iter() {
                    let mut lname = AString::with_capacity_in(raw_attr.name.len(), alloc);
                    for ch in raw_attr.name.chars() {
                        lname.push(ch.to_ascii_lowercase());
                    }
                    let val = decode_entities(raw_attr.value, alloc);
                    doc.attrs.push(Attr { name: lname, value: val });
                    attr_count = attr_count.saturating_add(1);
                }
                let mut tag_name = AString::with_capacity_in(name.len(), alloc);
                for ch in name.chars() {
                    tag_name.push(ch.to_ascii_lowercase());
                }

                let parent = open.last().map_or(NodeId::ROOT, |&(id, _)| id);
                if let Some(new_id) = doc.append_node(
                    parent,
                    NodeKind::Element { tag, tag_name, attr_start, attr_len: attr_count },
                ) && !self_closing
                    && !tag.is_void()
                    && open.len() < limits.max_depth
                {
                    open.push((new_id, tag));
                }
            }
            Token::EndTag { name } => {
                let tag = Tag::from_name(name);
                if tag.is_void() {
                    continue;
                }
                // Find the closest matching open ancestor (above the document root at index 0).
                let mut match_idx: Option<usize> = None;
                let mut idx = open.len();
                while idx > 1 {
                    idx = idx.saturating_sub(1);
                    if let Some(&(nid, open_tag)) = open.get(idx) {
                        let matched = if tag != Tag::Unknown {
                            open_tag == tag
                        } else if let Some(node) = doc.node(nid)
                            && let NodeKind::Element { ref tag_name, .. } = node.kind
                        {
                            tag_name.as_str().eq_ignore_ascii_case(name)
                        } else {
                            false
                        };
                        if matched {
                            match_idx = Some(idx);
                            break;
                        }
                    }
                }
                if let Some(cut) = match_idx {
                    while open.len() > cut {
                        let _ = open.pop();
                    }
                }
            }
        }
    }

    doc
}

fn apply_implicit_closes(open: &mut AVec<'_, (NodeId, Tag)>, incoming: Tag) {
    if incoming.closes_p()
        && let Some(&(_, Tag::P)) = open.last()
    {
        let _ = open.pop();
    }
    match incoming {
        Tag::Li => {
            if let Some(&(_, Tag::Li)) = open.last() {
                let _ = open.pop();
            }
        }
        Tag::Dt | Tag::Dd => {
            if let Some(&(_, Tag::Dt | Tag::Dd)) = open.last() {
                let _ = open.pop();
            }
        }
        Tag::Td | Tag::Th => {
            if let Some(&(_, Tag::Td | Tag::Th)) = open.last() {
                let _ = open.pop();
            }
        }
        Tag::Tr => {
            while let Some(&(_, Tag::Td | Tag::Th | Tag::Tr)) = open.last() {
                let _ = open.pop();
            }
        }
        Tag::Thead | Tag::Tbody | Tag::Tfoot => {
            while let Some(&(_, Tag::Td | Tag::Th | Tag::Tr | Tag::Thead | Tag::Tbody | Tag::Tfoot)) =
                open.last()
            {
                let _ = open.pop();
            }
        }
        _ => {}
    }
}

fn media_matches_scheme(media: &str, scheme: ColorScheme) -> bool {
    let trimmed = media.trim();
    if trimmed.is_empty() {
        return true;
    }
    let has_dark = contains_ascii_ci(trimmed, "prefers-color-scheme")
        && contains_ascii_ci(trimmed, "dark");
    let has_light = contains_ascii_ci(trimmed, "prefers-color-scheme")
        && contains_ascii_ci(trimmed, "light");
    match scheme {
        ColorScheme::Dark => has_dark || !has_light,
        ColorScheme::Light => has_light || !has_dark,
    }
}

fn first_srcset_url(srcset: &str) -> Option<&str> {
    let first_entry = srcset.split(',').next()?.trim();
    let url = first_entry.split_whitespace().next()?.trim();
    if url.is_empty() { None } else { Some(url) }
}

fn parse_px_dim(raw: &str) -> Option<u32> {
    let trimmed = raw.trim().trim_end_matches("px").trim();
    if trimmed.is_empty() {
        return None;
    }
    let int_part = trimmed.split('.').next().unwrap_or(trimmed);
    int_part.parse::<u32>().ok().filter(|&v| v > 0)
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    let limit = h.len().saturating_sub(n.len());
    let mut i = 0usize;
    while i <= limit {
        let mut matched = true;
        let mut j = 0usize;
        while j < n.len() {
            let hb = h.get(i.saturating_add(j)).copied().unwrap_or(0);
            let nb = n.get(j).copied().unwrap_or(0);
            if !hb.eq_ignore_ascii_case(&nb) {
                matched = false;
                break;
            }
            j = j.saturating_add(1);
        }
        if matched {
            return true;
        }
        i = i.saturating_add(1);
    }
    false
}
