//! Sans-IO HTML-to-visual-blocks renderer: lowers a [`Document`] into layout-ready
//! [`RenderBlock`]s using an explicit stack worklist (zero recursion, zero IO, bounded output).

use blaze_own::{AString, AVec, Alloc};

use crate::dom::{ColorScheme, Document, HtmlLimits, NodeId, NodeKind, Tag, TextAlign, parse_html};

/// Composable inline text styling flags (supports nested `<b><i>`, `<a><code>`, `<kbd>`, etc.).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub kbd: bool,
    pub strike: bool,
    pub underline: bool,
    pub mark: bool,
    pub link: bool,
}

/// One styled inline span inside a rendered HTML block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderSpan<'a> {
    pub text: AString<'a>,
    pub style: InlineStyle,
    pub href: Option<AString<'a>>,
}

/// Structured HTML table ready for direct grid/column layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderTable<'a> {
    pub headers: AVec<'a, AVec<'a, RenderSpan<'a>>>,
    pub alignments: AVec<'a, TextAlign>,
    pub rows: AVec<'a, AVec<'a, AVec<'a, RenderSpan<'a>>>>,
}

/// One layout-ready visual block emitted by [`render_document`] / [`render_html`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderBlock<'a> {
    Heading {
        level: u8,
        align: TextAlign,
        spans: AVec<'a, RenderSpan<'a>>,
    },
    Paragraph {
        align: TextAlign,
        spans: AVec<'a, RenderSpan<'a>>,
    },
    Image {
        src: AString<'a>,
        alt: AString<'a>,
        width: Option<u32>,
        height: Option<u32>,
        align: TextAlign,
    },
    Bullet {
        indent: u8,
        marker: AString<'a>,
        spans: AVec<'a, RenderSpan<'a>>,
    },
    Quote {
        spans: AVec<'a, RenderSpan<'a>>,
    },
    CodeFence {
        lang: AString<'a>,
        code: AString<'a>,
    },
    Table(RenderTable<'a>),
    Details {
        summary: AVec<'a, RenderSpan<'a>>,
        open: bool,
    },
    Rule,
    Blank,
}

/// Parse HTML `src` and immediately lower it into up to `max_blocks` [`RenderBlock`]s.
pub fn render_html<'a>(
    src: &str,
    scheme: ColorScheme,
    limits: HtmlLimits,
    max_blocks: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, RenderBlock<'a>> {
    let doc = parse_html(src, limits, alloc);
    render_document(&doc, scheme, max_blocks, alloc)
}

#[derive(Clone, Copy)]
enum ListKind {
    Unordered,
    Ordered(u32),
}

#[derive(Clone, Copy)]
enum Frame {
    Enter(NodeId),
    Exit {
        tag: Tag,
        prev_align: TextAlign,
        prev_style: InlineStyle,
        had_href: bool,
        pushed_list: bool,
        entered_quote: bool,
        entered_heading: bool,
        entered_li: bool,
        entered_p: bool,
    },
}

/// Lower an already-parsed [`Document`] into layout-ready [`RenderBlock`]s via an explicit worklist.
pub fn render_document<'a>(
    doc: &Document<'a>,
    scheme: ColorScheme,
    max_blocks: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, RenderBlock<'a>> {
    let mut blocks: AVec<'a, RenderBlock<'a>> = AVec::new_in(alloc);
    let mut cur_spans: AVec<'a, RenderSpan<'a>> = AVec::new_in(alloc);
    let mut cur_align = TextAlign::Left;
    let mut cur_style = InlineStyle::default();
    let mut href_stack: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    let mut list_stack: AVec<'a, ListKind> = AVec::new_in(alloc);
    let mut quote_depth: u8 = 0;
    let mut active_heading: Option<u8> = None;
    let mut pending_li_marker: Option<(u8, AString<'a>)> = None;

    let mut work: AVec<'a, Frame> = AVec::new_in(alloc);
    work.push(Frame::Enter(NodeId::ROOT));

    while let Some(frame) = work.pop() {
        if blocks.len() >= max_blocks {
            break;
        }
        match frame {
            Frame::Exit {
                tag,
                prev_align,
                prev_style,
                had_href,
                pushed_list,
                entered_quote,
                entered_heading,
                entered_li,
                entered_p,
            } => {
                if entered_heading {
                    if let Some(level) = active_heading.take() {
                        flush_spans(
                            &mut blocks,
                            &mut cur_spans,
                            FlushTarget::Heading(level, cur_align),
                            max_blocks,
                            alloc,
                        );
                    }
                } else if entered_li {
                    if let Some((indent, marker)) = pending_li_marker.take() {
                        flush_spans(
                            &mut blocks,
                            &mut cur_spans,
                            FlushTarget::Bullet(indent, marker),
                            max_blocks,
                            alloc,
                        );
                    } else if !cur_spans.is_empty() {
                        flush_spans(
                            &mut blocks,
                            &mut cur_spans,
                            FlushTarget::Paragraph(cur_align),
                            max_blocks,
                            alloc,
                        );
                    }
                } else if entered_p || matches!(tag, Tag::Div | Tag::Center | Tag::Figcaption | Tag::Dt | Tag::Dd) {
                    if !cur_spans.is_empty() {
                        let target = if quote_depth > 0 {
                            FlushTarget::Quote
                        } else if let Some((indent, marker)) = pending_li_marker.take() {
                            FlushTarget::Bullet(indent, marker)
                        } else {
                            FlushTarget::Paragraph(cur_align)
                        };
                        flush_spans(&mut blocks, &mut cur_spans, target, max_blocks, alloc);
                    }
                } else if entered_quote {
                    if !cur_spans.is_empty() {
                        flush_spans(&mut blocks, &mut cur_spans, FlushTarget::Quote, max_blocks, alloc);
                    }
                    quote_depth = quote_depth.saturating_sub(1);
                }

                if pushed_list {
                    let _ = list_stack.pop();
                }
                if had_href {
                    let _ = href_stack.pop();
                }
                cur_align = prev_align;
                cur_style = prev_style;
            }

            Frame::Enter(id) => {
                let Some(node) = doc.node(id) else {
                    continue;
                };
                match &node.kind {
                    NodeKind::Comment => {}
                    NodeKind::Document => {
                        push_children_rev(doc, id, &mut work, alloc);
                    }
                    NodeKind::Text(text) => {
                        let href = href_stack.last().map(|s| s.as_str());
                        append_collapsed_text(&mut cur_spans, text.as_str(), cur_style, href, alloc);
                    }
                    NodeKind::Element { tag, .. } => {
                        let tag = *tag;
                        if tag.is_non_visual() {
                            continue;
                        }

                        // Special self-contained visual elements handled directly:
                        if tag == Tag::Br {
                            // Emit a line break inside the current block!
                            if !cur_spans.is_empty() {
                                let target = if let Some(level) = active_heading {
                                    FlushTarget::Heading(level, cur_align)
                                } else if quote_depth > 0 {
                                    FlushTarget::Quote
                                } else if let Some((indent, marker)) = pending_li_marker.take() {
                                    FlushTarget::Bullet(indent, marker)
                                } else {
                                    FlushTarget::Paragraph(cur_align)
                                };
                                flush_spans(&mut blocks, &mut cur_spans, target, max_blocks, alloc);
                            }
                            continue;
                        }

                        if tag == Tag::Hr {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                cur_align,
                                max_blocks,
                                alloc,
                            );
                            if blocks.len() < max_blocks {
                                blocks.push(RenderBlock::Rule);
                            }
                            continue;
                        }

                        if tag == Tag::Picture {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                cur_align,
                                max_blocks,
                                alloc,
                            );
                            let align = doc.element_align(id).unwrap_or(cur_align);
                            if let Some(choice) = doc.resolve_picture(id, scheme, alloc)
                                && blocks.len() < max_blocks
                            {
                                blocks.push(RenderBlock::Image {
                                    src: choice.src,
                                    alt: choice.alt,
                                    width: choice.width,
                                    height: choice.height,
                                    align,
                                });
                            }
                            continue;
                        }

                        if tag == Tag::Img {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                cur_align,
                                max_blocks,
                                alloc,
                            );
                            let align = doc.element_align(id).unwrap_or(cur_align);
                            if let Some(choice) = doc.resolve_img(id, alloc)
                                && blocks.len() < max_blocks
                            {
                                blocks.push(RenderBlock::Image {
                                    src: choice.src,
                                    alt: choice.alt,
                                    width: choice.width,
                                    height: choice.height,
                                    align,
                                });
                            }
                            continue;
                        }

                        if tag == Tag::Pre {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                cur_align,
                                max_blocks,
                                alloc,
                            );
                            let (lang, code) = collect_pre_code(doc, id, alloc);
                            if blocks.len() < max_blocks {
                                blocks.push(RenderBlock::CodeFence { lang, code });
                            }
                            continue;
                        }

                        if tag == Tag::Table {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                cur_align,
                                max_blocks,
                                alloc,
                            );
                            if let Some(tbl) = collect_table(doc, id, alloc)
                                && blocks.len() < max_blocks
                            {
                                blocks.push(RenderBlock::Table(tbl));
                            }
                            continue;
                        }

                        if tag == Tag::Summary {
                            let summary = collect_inline_spans(doc, id, alloc);
                            if !summary.is_empty() && blocks.len() < max_blocks {
                                let open_flag = node
                                    .parent
                                    .is_some_and(|pid| doc.attr(pid, "open").is_some());
                                blocks.push(RenderBlock::Details { summary, open: open_flag });
                            }
                            continue;
                        }

                        let prev_align = cur_align;
                        let prev_style = cur_style;
                        let mut had_href = false;
                        let mut pushed_list = false;
                        let mut entered_quote = false;
                        let mut entered_heading = false;
                        let mut entered_li = false;
                        let mut entered_p = false;

                        if let Some(a) = doc.element_align(id) {
                            cur_align = a;
                        }

                        if let Some(level) = tag.heading_level() {
                            flush_open(
                                &mut blocks,
                                &mut cur_spans,
                                active_heading,
                                quote_depth,
                                &mut pending_li_marker,
                                prev_align,
                                max_blocks,
                                alloc,
                            );
                            active_heading = Some(level);
                            entered_heading = true;
                        } else {
                            match tag {
                                Tag::P => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                    entered_p = true;
                                }
                                Tag::Div | Tag::Center | Tag::Section | Tag::Article | Tag::Header | Tag::Footer | Tag::Main | Tag::Figure | Tag::Figcaption | Tag::Details => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                }
                                Tag::Blockquote => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                    quote_depth = quote_depth.saturating_add(1);
                                    entered_quote = true;
                                }
                                Tag::Ul | Tag::Dl => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                    list_stack.push(ListKind::Unordered);
                                    pushed_list = true;
                                }
                                Tag::Ol => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                    let start = doc
                                        .attr(id, "start")
                                        .and_then(|s| s.trim().parse::<u32>().ok())
                                        .unwrap_or(1);
                                    list_stack.push(ListKind::Ordered(start));
                                    pushed_list = true;
                                }
                                Tag::Li => {
                                    flush_open(
                                        &mut blocks,
                                        &mut cur_spans,
                                        active_heading,
                                        quote_depth,
                                        &mut pending_li_marker,
                                        prev_align,
                                        max_blocks,
                                        alloc,
                                    );
                                    let indent = u8::try_from(list_stack.len().saturating_sub(1))
                                        .unwrap_or(4)
                                        .min(4);
                                    let marker = match list_stack.last_mut() {
                                        Some(ListKind::Ordered(n)) => {
                                            let cur_n = *n;
                                            *n = n.saturating_add(1);
                                            format_ordered_marker(cur_n, alloc)
                                        }
                                        _ => AString::from_str_in("•", alloc),
                                    };
                                    pending_li_marker = Some((indent, marker));
                                    entered_li = true;
                                }
                                Tag::Dt => {
                                    cur_style.bold = true;
                                }
                                Tag::Strong | Tag::B => {
                                    cur_style.bold = true;
                                }
                                Tag::Em | Tag::I => {
                                    cur_style.italic = true;
                                }
                                Tag::Code | Tag::Samp | Tag::Var => {
                                    cur_style.code = true;
                                }
                                Tag::Kbd => {
                                    cur_style.kbd = true;
                                }
                                Tag::S | Tag::Del => {
                                    cur_style.strike = true;
                                }
                                Tag::U | Tag::Ins => {
                                    cur_style.underline = true;
                                }
                                Tag::Mark => {
                                    cur_style.mark = true;
                                }
                                Tag::A => {
                                    cur_style.link = true;
                                    if let Some(h) = doc.attr(id, "href") {
                                        href_stack.push(AString::from_str_in(h, alloc));
                                        had_href = true;
                                    }
                                }
                                _ => {}
                            }
                        }

                        work.push(Frame::Exit {
                            tag,
                            prev_align,
                            prev_style,
                            had_href,
                            pushed_list,
                            entered_quote,
                            entered_heading,
                            entered_li,
                            entered_p,
                        });
                        push_children_rev(doc, id, &mut work, alloc);
                    }
                }
            }
        }
    }

    if !cur_spans.is_empty() && blocks.len() < max_blocks {
        flush_open(
            &mut blocks,
            &mut cur_spans,
            active_heading,
            quote_depth,
            &mut pending_li_marker,
            cur_align,
            max_blocks,
            alloc,
        );
    }

    blocks
}

enum FlushTarget<'a> {
    Heading(u8, TextAlign),
    Paragraph(TextAlign),
    Bullet(u8, AString<'a>),
    Quote,
}

#[allow(clippy::too_many_arguments)]
fn flush_open<'a>(
    blocks: &mut AVec<'a, RenderBlock<'a>>,
    cur_spans: &mut AVec<'a, RenderSpan<'a>>,
    active_heading: Option<u8>,
    quote_depth: u8,
    pending_li_marker: &mut Option<(u8, AString<'a>)>,
    cur_align: TextAlign,
    max_blocks: usize,
    alloc: Alloc<'a>,
) {
    if cur_spans.is_empty() {
        return;
    }
    let target = if let Some(level) = active_heading {
        FlushTarget::Heading(level, cur_align)
    } else if quote_depth > 0 {
        FlushTarget::Quote
    } else if let Some((indent, marker)) = pending_li_marker.take() {
        FlushTarget::Bullet(indent, marker)
    } else {
        FlushTarget::Paragraph(cur_align)
    };
    flush_spans(blocks, cur_spans, target, max_blocks, alloc);
}

fn flush_spans<'a>(
    blocks: &mut AVec<'a, RenderBlock<'a>>,
    cur_spans: &mut AVec<'a, RenderSpan<'a>>,
    target: FlushTarget<'a>,
    max_blocks: usize,
    alloc: Alloc<'a>,
) {
    trim_span_edges(cur_spans);
    if cur_spans.is_empty() || blocks.len() >= max_blocks {
        cur_spans.clear();
        return;
    }
    let mut taken = AVec::with_capacity_in(cur_spans.len(), alloc);
    core::mem::swap(&mut taken, cur_spans);
    let blk = match target {
        FlushTarget::Heading(level, align) => RenderBlock::Heading { level, align, spans: taken },
        FlushTarget::Paragraph(align) => RenderBlock::Paragraph { align, spans: taken },
        FlushTarget::Bullet(indent, marker) => RenderBlock::Bullet { indent, marker, spans: taken },
        FlushTarget::Quote => RenderBlock::Quote { spans: taken },
    };
    blocks.push(blk);
}

fn trim_span_edges(spans: &mut AVec<'_, RenderSpan<'_>>) {
    // Trim leading spaces from first span (if not code/kbd).
    while let Some(first) = spans.first_mut() {
        if first.style.code || first.style.kbd {
            break;
        }
        let trimmed_len = first.text.as_str().trim_start().len();
        if trimmed_len == 0 {
            let _ = spans.remove_shift(0);
        } else if trimmed_len < first.text.len() {
            let cut = first.text.len().saturating_sub(trimmed_len);
            if let Some(rest) = first.text.as_str().get(cut..) {
                let mut shifted = AString::with_capacity_in(rest.len(), *first.text.allocator());
                shifted.push_str(rest);
                first.text = shifted;
            }
            break;
        } else {
            break;
        }
    }
    // Trim trailing spaces from last span (if not code/kbd).
    while let Some(last) = spans.last_mut() {
        if last.style.code || last.style.kbd {
            break;
        }
        let trimmed_len = last.text.as_str().trim_end().len();
        if trimmed_len == 0 {
            let _ = spans.pop();
        } else {
            last.text.truncate(trimmed_len);
            break;
        }
    }
}

fn push_children_rev<'a>(doc: &Document<'a>, parent: NodeId, work: &mut AVec<'a, Frame>, alloc: Alloc<'a>) {
    let mut children: AVec<'a, NodeId> = AVec::new_in(alloc);
    let mut cur = doc.node(parent).and_then(|n| n.first_child);
    while let Some(cid) = cur {
        children.push(cid);
        cur = doc.node(cid).and_then(|n| n.next_sibling);
    }
    while let Some(cid) = children.pop() {
        work.push(Frame::Enter(cid));
    }
}

fn append_collapsed_text<'a>(
    spans: &mut AVec<'a, RenderSpan<'a>>,
    raw: &str,
    style: InlineStyle,
    href: Option<&str>,
    alloc: Alloc<'a>,
) {
    let mut buf = AString::with_capacity_in(raw.len(), alloc);
    let mut prev_ws = spans
        .last()
        .is_some_and(|s| s.text.as_str().ends_with(' '));
    let at_start = spans.is_empty();

    for ch in raw.chars() {
        if ch == '\u{00A0}' {
            buf.push(' ');
            prev_ws = false;
        } else if ch.is_ascii_whitespace() {
            if !prev_ws && !(at_start && buf.is_empty()) {
                buf.push(' ');
                prev_ws = true;
            }
        } else {
            buf.push(ch);
            prev_ws = false;
        }
    }

    if buf.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
        && last.href.as_deref() == href
    {
        last.text.push_str(buf.as_str());
        return;
    }
    spans.push(RenderSpan {
        text: buf,
        style,
        href: href.map(|h| AString::from_str_in(h, alloc)),
    });
}

fn collect_pre_code<'a>(doc: &Document<'a>, pre_id: NodeId, alloc: Alloc<'a>) -> (AString<'a>, AString<'a>) {
    let mut lang = AString::new_in(alloc);
    let mut code = AString::new_in(alloc);
    let mut stack: AVec<'a, NodeId> = AVec::new_in(alloc);
    stack.push(pre_id);

    while let Some(nid) = stack.pop() {
        let Some(node) = doc.node(nid) else {
            continue;
        };
        match &node.kind {
            NodeKind::Text(t) => code.push_str(t.as_str()),
            NodeKind::Element { tag, .. } => {
                if *tag == Tag::Code && lang.is_empty()
                    && let Some(cls) = doc.attr(nid, "class")
                {
                    for word in cls.split_whitespace() {
                        if let Some(rest) = word
                            .strip_prefix("language-")
                            .or_else(|| word.strip_prefix("lang-"))
                        {
                            lang.push_str(rest);
                            break;
                        }
                    }
                }
                let mut kids: AVec<'a, NodeId> = AVec::new_in(alloc);
                let mut cur = node.first_child;
                while let Some(cid) = cur {
                    kids.push(cid);
                    cur = doc.node(cid).and_then(|c| c.next_sibling);
                }
                while let Some(cid) = kids.pop() {
                    stack.push(cid);
                }
            }
            _ => {}
        }
    }
    // Trim a single leading/trailing newline inside `<pre><code>\n...\n</code></pre>`.
    let trimmed = code.as_str().trim_matches('\n');
    let mut out = AString::with_capacity_in(trimmed.len(), alloc);
    out.push_str(trimmed);
    (lang, out)
}

fn collect_inline_spans<'a>(doc: &Document<'a>, root: NodeId, alloc: Alloc<'a>) -> AVec<'a, RenderSpan<'a>> {
    let mut spans: AVec<'a, RenderSpan<'a>> = AVec::new_in(alloc);
    let mut style = InlineStyle::default();
    let mut href_stack: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    let mut stack: AVec<'a, (NodeId, bool, InlineStyle, bool)> = AVec::new_in(alloc);

    let mut kids: AVec<'a, NodeId> = AVec::new_in(alloc);
    let mut cur = doc.node(root).and_then(|n| n.first_child);
    while let Some(cid) = cur {
        kids.push(cid);
        cur = doc.node(cid).and_then(|c| c.next_sibling);
    }
    while let Some(cid) = kids.pop() {
        stack.push((cid, true, style, false));
    }

    while let Some((nid, entering, prev_style, had_href)) = stack.pop() {
        if !entering {
            style = prev_style;
            if had_href {
                let _ = href_stack.pop();
            }
            continue;
        }
        let Some(node) = doc.node(nid) else {
            continue;
        };
        match &node.kind {
            NodeKind::Text(t) => {
                let href = href_stack.last().map(|s| s.as_str());
                append_collapsed_text(&mut spans, t.as_str(), style, href, alloc);
            }
            NodeKind::Element { tag, .. } => {
                if tag.is_non_visual() {
                    continue;
                }
                let saved = style;
                let mut pushed_href = false;
                match *tag {
                    Tag::Strong | Tag::B => style.bold = true,
                    Tag::Em | Tag::I => style.italic = true,
                    Tag::Code | Tag::Samp | Tag::Var => style.code = true,
                    Tag::Kbd => style.kbd = true,
                    Tag::S | Tag::Del => style.strike = true,
                    Tag::U | Tag::Ins => style.underline = true,
                    Tag::Mark => style.mark = true,
                    Tag::A => {
                        style.link = true;
                        if let Some(h) = doc.attr(nid, "href") {
                            href_stack.push(AString::from_str_in(h, alloc));
                            pushed_href = true;
                        }
                    }
                    Tag::Br => {
                        append_collapsed_text(&mut spans, " ", style, None, alloc);
                        continue;
                    }
                    _ => {}
                }
                stack.push((nid, false, saved, pushed_href));
                let mut sub: AVec<'a, NodeId> = AVec::new_in(alloc);
                let mut c = node.first_child;
                while let Some(cid) = c {
                    sub.push(cid);
                    c = doc.node(cid).and_then(|cn| cn.next_sibling);
                }
                while let Some(cid) = sub.pop() {
                    stack.push((cid, true, style, false));
                }
            }
            _ => {}
        }
    }
    trim_span_edges(&mut spans);
    spans
}

fn collect_table<'a>(doc: &Document<'a>, table_id: NodeId, alloc: Alloc<'a>) -> Option<RenderTable<'a>> {
    let mut headers: AVec<'a, AVec<'a, RenderSpan<'a>>> = AVec::new_in(alloc);
    let mut alignments: AVec<'a, TextAlign> = AVec::new_in(alloc);
    let mut rows: AVec<'a, AVec<'a, AVec<'a, RenderSpan<'a>>>> = AVec::new_in(alloc);

    // Gather `<tr>` elements in document order via an explicit stack.
    let mut tr_ids: AVec<'a, NodeId> = AVec::new_in(alloc);
    let mut stack: AVec<'a, NodeId> = AVec::new_in(alloc);
    stack.push(table_id);
    while let Some(nid) = stack.pop() {
        let Some(node) = doc.node(nid) else {
            continue;
        };
        if nid != table_id
            && let NodeKind::Element { tag, .. } = node.kind
        {
            if tag == Tag::Tr {
                tr_ids.push(nid);
                continue;
            }
            if tag == Tag::Table {
                continue; // Do not descend into nested tables.
            }
        }
        let mut kids: AVec<'a, NodeId> = AVec::new_in(alloc);
        let mut c = node.first_child;
        while let Some(cid) = c {
            kids.push(cid);
            c = doc.node(cid).and_then(|cn| cn.next_sibling);
        }
        while let Some(cid) = kids.pop() {
            stack.push(cid);
        }
    }

    for &tr_id in tr_ids.iter() {
        let mut is_header_row = doc
            .node(tr_id)
            .and_then(|n| n.parent)
            .and_then(|pid| doc.node(pid))
            .is_some_and(|p| matches!(p.kind, NodeKind::Element { tag: Tag::Thead, .. }));
        let mut cells: AVec<'a, AVec<'a, RenderSpan<'a>>> = AVec::new_in(alloc);
        let mut cell_aligns: AVec<'a, TextAlign> = AVec::new_in(alloc);
        let mut saw_th = false;

        let mut c = doc.node(tr_id).and_then(|n| n.first_child);
        while let Some(cid) = c {
            let Some(cell_node) = doc.node(cid) else {
                break;
            };
            if let NodeKind::Element { tag: Tag::Th | Tag::Td, .. } = cell_node.kind {
                if matches!(cell_node.kind, NodeKind::Element { tag: Tag::Th, .. }) {
                    saw_th = true;
                }
                cells.push(collect_inline_spans(doc, cid, alloc));
                cell_aligns.push(doc.element_align(cid).unwrap_or(TextAlign::Left));
            }
            c = cell_node.next_sibling;
        }
        if cells.is_empty() {
            continue;
        }
        if saw_th && headers.is_empty() && rows.is_empty() {
            is_header_row = true;
        }
        if is_header_row && headers.is_empty() {
            headers = cells;
            alignments = cell_aligns;
        } else {
            if alignments.len() < cell_aligns.len() {
                for &al in cell_aligns.iter().skip(alignments.len()) {
                    alignments.push(al);
                }
            }
            rows.push(cells);
        }
    }

    if headers.is_empty() && rows.is_empty() {
        return None;
    }
    Some(RenderTable { headers, alignments, rows })
}

fn format_ordered_marker<'a>(n: u32, alloc: Alloc<'a>) -> AString<'a> {
    let mut digits = [0u8; 12];
    let mut idx = digits.len();
    let mut val = n;
    if val == 0 {
        idx = idx.saturating_sub(1);
        if let Some(slot) = digits.get_mut(idx) {
            *slot = b'0';
        }
    } else {
        while val > 0 && idx > 0 {
            idx = idx.saturating_sub(1);
            let rem = u8::try_from(val % 10).unwrap_or(0);
            if let Some(slot) = digits.get_mut(idx) {
                *slot = b'0'.saturating_add(rem);
            }
            val /= 10;
        }
    }
    let mut out = AString::with_capacity_in(14, alloc);
    if let Some(slice) = digits.get(idx..).and_then(|s| core::str::from_utf8(s).ok()) {
        out.push_str(slice);
    }
    out.push('.');
    out
}
