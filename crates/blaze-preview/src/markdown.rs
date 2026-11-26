//! Pure `#![no_std]` unified Markdown + embedded HTML renderer model for `blaze-preview`:
//! parses GFM headings, soft-wrapped paragraphs, bullet/numbered lists, GFM pipe tables,
//! syntax-highlighted fenced code blocks, blockquotes, horizontal rules, images, inline
//! **bold**, *italic*, ~~strike~~, `` `code` ``, `<kbd>`, links, AND delegates embedded HTML
//! blocks/elements (`<picture>` dark/light selection, `<p align="center">`, `<h3 align="center">`,
//! `<code>`, `<a href="...">`, `<br>`, `<table>`, `<details>`, HTML comments & entities) to
//! `blaze-html` for direct GPU rendering.

use blaze_html::{
    ColorScheme, HtmlLimits, InlineStyle, RenderBlock, RenderSpan, RenderTable, TextAlign,
    decode_entities, render_html,
};
use blaze_own::{AString, AVec, Alloc};

use crate::highlight::{CodeLine, highlight_code};

/// Inline text style inside a rendered Markdown/HTML paragraph, heading, list item, or table cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdStyle {
    Plain,
    Bold,
    Italic,
    BoldItalic,
    Code,
    Kbd,
    Strike,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdSpan<'a> {
    pub text: AString<'a>,
    pub style: MdStyle,
    pub href: Option<AString<'a>>,
}

/// Structured GFM or HTML table inside a rendered document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdTable<'a> {
    pub headers: AVec<'a, AVec<'a, MdSpan<'a>>>,
    pub alignments: AVec<'a, TextAlign>,
    pub rows: AVec<'a, AVec<'a, AVec<'a, MdSpan<'a>>>>,
}

/// One visual block in a rendered Markdown / HTML document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdBlock<'a> {
    Heading {
        level: u8,
        align: TextAlign,
        spans: AVec<'a, MdSpan<'a>>,
    },
    Paragraph {
        align: TextAlign,
        spans: AVec<'a, MdSpan<'a>>,
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
        spans: AVec<'a, MdSpan<'a>>,
    },
    Quote {
        spans: AVec<'a, MdSpan<'a>>,
    },
    CodeFence {
        lang: AString<'a>,
        lines: AVec<'a, CodeLine<'a>>,
    },
    Table(MdTable<'a>),
    Details {
        summary: AVec<'a, MdSpan<'a>>,
        open: bool,
    },
    Rule,
    Blank,
}

/// Parse standalone HTML into unified [`MdBlock`]s (with syntax-highlighted `<pre><code>`).
pub fn parse_html_document<'a>(
    src: &str,
    scheme: ColorScheme,
    max_blocks: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, MdBlock<'a>> {
    let raw = render_html(src, scheme, HtmlLimits::SHIPPED, max_blocks, alloc);
    let mut out = AVec::with_capacity_in(raw.len(), alloc);
    lower_html_blocks(raw, &mut out, max_blocks, alloc);
    out
}

/// Parse up to `max_blocks` rendered Markdown + embedded HTML blocks from `src` into `alloc`.
pub fn parse_markdown<'a>(src: &str, max_blocks: usize, alloc: Alloc<'a>) -> AVec<'a, MdBlock<'a>> {
    parse_markdown_with_scheme(src, ColorScheme::Dark, max_blocks, alloc)
}

/// Parse Markdown with explicit [`ColorScheme`] for embedded `<picture>` dark/light selection.
pub fn parse_markdown_with_scheme<'a>(
    src: &str,
    scheme: ColorScheme,
    max_blocks: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, MdBlock<'a>> {
    let cleaned = strip_html_comments(src, alloc);
    let mut blocks = AVec::new_in(alloc);
    let mut raw_lines: AVec<'a, &str> = AVec::new_in(alloc);
    for l in cleaned.as_str().lines() {
        raw_lines.push(l);
    }

    let total = raw_lines.len();
    let mut idx = 0usize;

    // Skip YAML front matter (`---` at very start of document).
    if raw_lines.first().is_some_and(|l| l.trim() == "---") {
        let mut fm_end = None;
        let mut j = 1usize;
        while j < total {
            let t = raw_lines.get(j).copied().unwrap_or("").trim();
            if t == "---" || t == "..." {
                fm_end = Some(j.saturating_add(1));
                break;
            }
            j = j.saturating_add(1);
        }
        if let Some(after) = fm_end {
            idx = after;
        }
    }

    while idx < total && blocks.len() < max_blocks {
        let raw = raw_lines.get(idx).copied().unwrap_or("");
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            if !matches!(blocks.last(), None | Some(MdBlock::Blank)) {
                blocks.push(MdBlock::Blank);
            }
            idx = idx.saturating_add(1);
            continue;
        }

        // 1. Fenced code block (` ```lang ... ``` `)
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang_label = normalize_code_lang(rest.trim());
            let mut body = AString::new_in(alloc);
            let mut count = 0usize;
            idx = idx.saturating_add(1);
            while idx < total {
                let code_line = raw_lines.get(idx).copied().unwrap_or("");
                idx = idx.saturating_add(1);
                if code_line.trim().starts_with("```") {
                    break;
                }
                if count < 48 {
                    if count > 0 {
                        body.push('\n');
                    }
                    body.push_str(code_line);
                    count = count.saturating_add(1);
                }
            }
            let hl = highlight_code(body.as_str(), lang_label, 48, alloc);
            blocks.push(MdBlock::CodeFence {
                lang: AString::from_str_in(lang_label, alloc),
                lines: hl,
            });
            continue;
        }

        // 2. Embedded HTML block (`<p ...>`, `<picture>`, `<h3 align="center">`, `<table>`, etc.)
        if let Some(open_tag) = starts_html_block(trimmed) {
            let mut html_chunk = AString::new_in(alloc);
            let mut depth: i32 = 0;
            let mut first = true;
            while idx < total {
                let line = raw_lines.get(idx).copied().unwrap_or("");
                let lt = line.trim();
                if !first && lt.is_empty() && depth <= 0 {
                    break;
                }
                if !first {
                    html_chunk.push('\n');
                }
                html_chunk.push_str(line);
                depth = depth.saturating_add(tag_balance_delta(lt, open_tag));
                idx = idx.saturating_add(1);
                first = false;
                // Void/self-closing single-line tags (`<img ...>`, `<hr>`) or balanced block end immediately.
                if is_void_block_tag(open_tag) || (depth <= 0 && has_closing_tag(lt, open_tag)) {
                    break;
                }
            }
            let rem = max_blocks.saturating_sub(blocks.len());
            let rendered = render_html(html_chunk.as_str(), scheme, HtmlLimits::SHIPPED, rem, alloc);
            lower_html_blocks(rendered, &mut blocks, max_blocks, alloc);
            continue;
        }

        // 3. Horizontal rule (`---`, `***`, `___`)
        if is_horizontal_rule(trimmed) {
            blocks.push(MdBlock::Rule);
            idx = idx.saturating_add(1);
            continue;
        }

        // 4. ATX Heading (`#`..`######`)
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes)
            && trimmed.as_bytes().get(hashes) == Some(&b' ')
        {
            let content = trimmed
                .get(hashes.saturating_add(1)..)
                .unwrap_or("")
                .trim()
                .trim_end_matches('#')
                .trim();
            blocks.push(MdBlock::Heading {
                level: u8::try_from(hashes).unwrap_or(1),
                align: TextAlign::Left,
                spans: parse_inline(content, alloc),
            });
            idx = idx.saturating_add(1);
            continue;
        }

        // 5. GFM Pipe Table (`| H1 | H2 |` followed by `|---|---:|`)
        if trimmed.contains('|')
            && idx.saturating_add(1) < total
            && let Some(aligns) = parse_gfm_table_sep(
                raw_lines.get(idx.saturating_add(1)).copied().unwrap_or(""),
                alloc,
            )
        {
            let header_cells = split_pipe_row(trimmed, alloc);
            let mut headers = AVec::with_capacity_in(header_cells.len(), alloc);
            for cell in header_cells.iter() {
                headers.push(parse_inline(cell, alloc));
            }
            idx = idx.saturating_add(2);
            let mut rows = AVec::new_in(alloc);
            while idx < total && rows.len() < 64 {
                let row_line = raw_lines.get(idx).copied().unwrap_or("").trim();
                if row_line.is_empty() || !row_line.contains('|') {
                    break;
                }
                let raw_cells = split_pipe_row(row_line, alloc);
                let mut cells = AVec::with_capacity_in(raw_cells.len(), alloc);
                for cell in raw_cells.iter() {
                    cells.push(parse_inline(cell, alloc));
                }
                rows.push(cells);
                idx = idx.saturating_add(1);
            }
            blocks.push(MdBlock::Table(MdTable { headers, alignments: aligns, rows }));
            continue;
        }

        // 6. Standalone Markdown image (`![alt](src)`)
        if let Some((alt, img_src)) = parse_standalone_md_image(trimmed) {
            blocks.push(MdBlock::Image {
                src: AString::from_str_in(img_src, alloc),
                alt: AString::from_str_in(alt, alloc),
                width: None,
                height: None,
                align: TextAlign::Left,
            });
            idx = idx.saturating_add(1);
            continue;
        }

        // 7. Blockquote (`> ...`) — join consecutive `>` lines into a clean wrapped quote!
        if let Some(first_rest) = trimmed.strip_prefix('>') {
            let mut joined = AString::from_str_in(first_rest.trim_start(), alloc);
            idx = idx.saturating_add(1);
            while idx < total {
                let nxt = raw_lines.get(idx).copied().unwrap_or("").trim();
                if let Some(rest) = nxt.strip_prefix('>') {
                    let rt = rest.trim_start();
                    if rt.is_empty() {
                        break;
                    }
                    if !joined.is_empty() {
                        joined.push(' ');
                    }
                    joined.push_str(rt);
                    idx = idx.saturating_add(1);
                } else {
                    break;
                }
            }
            blocks.push(MdBlock::Quote { spans: parse_inline(joined.as_str(), alloc) });
            continue;
        }

        // 8. Unordered bullet (`- `, `* `, `+ `) with wrapped continuation lines
        let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let mut body = AString::from_str_in(rest.trim(), alloc);
            idx = idx.saturating_add(1);
            while idx < total {
                let nxt_raw = raw_lines.get(idx).copied().unwrap_or("");
                let nxt = nxt_raw.trim();
                if nxt.is_empty() || starts_any_block(nxt) {
                    break;
                }
                // Continuation line of list item
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(nxt);
                idx = idx.saturating_add(1);
            }
            let indent = u8::try_from((leading_spaces / 2).min(4)).unwrap_or(0);
            blocks.push(MdBlock::Bullet {
                indent,
                marker: AString::from_str_in("•", alloc),
                spans: parse_inline(body.as_str(), alloc),
            });
            continue;
        }

        // 9. Ordered list (`1. `, `2. `) with wrapped continuation lines
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && digits <= 3
            && trimmed.get(digits..).is_some_and(|r| r.starts_with(". "))
        {
            let marker = trimmed.get(..=digits).unwrap_or("1.");
            let rest = trimmed.get(digits.saturating_add(2)..).unwrap_or("").trim();
            let mut body = AString::from_str_in(rest, alloc);
            idx = idx.saturating_add(1);
            while idx < total {
                let nxt = raw_lines.get(idx).copied().unwrap_or("").trim();
                if nxt.is_empty() || starts_any_block(nxt) {
                    break;
                }
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(nxt);
                idx = idx.saturating_add(1);
            }
            let indent = u8::try_from((leading_spaces / 2).min(4)).unwrap_or(0);
            blocks.push(MdBlock::Bullet {
                indent,
                marker: AString::from_str_in(marker, alloc),
                spans: parse_inline(body.as_str(), alloc),
            });
            continue;
        }

        // 10. Soft-wrapped paragraph (join consecutive prose lines cleanly, splitting on `<br>`).
        let mut para = AString::from_str_in(trimmed, alloc);
        idx = idx.saturating_add(1);
        while idx < total {
            let nxt = raw_lines.get(idx).copied().unwrap_or("").trim();
            if nxt.is_empty() || starts_any_block(nxt) {
                break;
            }
            // Don't swallow a GFM pipe table starting on the next line.
            if nxt.contains('|')
                && idx.saturating_add(1) < total
                && parse_gfm_table_sep(
                    raw_lines.get(idx.saturating_add(1)).copied().unwrap_or(""),
                    alloc,
                )
                .is_some()
            {
                break;
            }
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(nxt);
            idx = idx.saturating_add(1);
        }
        emit_paragraph_with_breaks(&mut blocks, para.as_str(), TextAlign::Left, max_blocks, alloc);
    }

    blocks
}

fn emit_paragraph_with_breaks<'a>(
    blocks: &mut AVec<'a, MdBlock<'a>>,
    text: &str,
    align: TextAlign,
    max_blocks: usize,
    alloc: Alloc<'a>,
) {
    // Split on inline `<br>`, `<br/>`, `<br />` (case-insensitive).
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len && blocks.len() < max_blocks {
        if let Some(br_len) = match_br_tag(text, i) {
            let seg = text.get(start..i).unwrap_or("").trim();
            if !seg.is_empty() {
                blocks.push(MdBlock::Paragraph { align, spans: parse_inline(seg, alloc) });
            }
            i = i.saturating_add(br_len);
            start = i;
            continue;
        }
        i = i.saturating_add(1);
    }
    if blocks.len() < max_blocks {
        let seg = text.get(start..).unwrap_or("").trim();
        if !seg.is_empty() {
            blocks.push(MdBlock::Paragraph { align, spans: parse_inline(seg, alloc) });
        }
    }
}

fn match_br_tag(text: &str, at: usize) -> Option<usize> {
    let rest = text.get(at..)?;
    if !rest.starts_with('<') {
        return None;
    }
    let after = rest.get(1..)?;
    if after.len() < 3 || !after.get(..2)?.eq_ignore_ascii_case("br") {
        return None;
    }
    let tail = after.get(2..)?;
    let mut k = 0usize;
    let tb = tail.as_bytes();
    while tb.get(k).copied().is_some_and(|b| b.is_ascii_whitespace()) {
        k = k.saturating_add(1);
    }
    if tb.get(k).copied() == Some(b'/') {
        k = k.saturating_add(1);
    }
    if tb.get(k).copied() == Some(b'>') {
        return Some(k.saturating_add(4));
    }
    None
}

fn strip_html_comments<'a>(src: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(src.len(), alloc);
    let mut rest = src;
    while let Some(pos) = rest.find("<!--") {
        if let Some(before) = rest.get(..pos) {
            out.push_str(before);
        }
        let after = rest.get(pos.saturating_add(4)..).unwrap_or("");
        if let Some(end) = after.find("-->") {
            rest = after.get(end.saturating_add(3)..).unwrap_or("");
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn lower_html_blocks<'a>(
    raw: AVec<'a, RenderBlock<'a>>,
    out: &mut AVec<'a, MdBlock<'a>>,
    max_blocks: usize,
    alloc: Alloc<'a>,
) {
    for blk in raw {
        if out.len() >= max_blocks {
            break;
        }
        match blk {
            RenderBlock::Heading { level, align, spans } => {
                out.push(MdBlock::Heading { level, align, spans: lower_spans(spans, alloc) });
            }
            RenderBlock::Paragraph { align, spans } => {
                out.push(MdBlock::Paragraph { align, spans: lower_spans(spans, alloc) });
            }
            RenderBlock::Image { src, alt, width, height, align } => {
                out.push(MdBlock::Image { src, alt, width, height, align });
            }
            RenderBlock::Bullet { indent, marker, spans } => {
                out.push(MdBlock::Bullet { indent, marker, spans: lower_spans(spans, alloc) });
            }
            RenderBlock::Quote { spans } => {
                out.push(MdBlock::Quote { spans: lower_spans(spans, alloc) });
            }
            RenderBlock::CodeFence { lang, code } => {
                let label = normalize_code_lang(lang.as_str());
                let lines = highlight_code(code.as_str(), label, 48, alloc);
                out.push(MdBlock::CodeFence {
                    lang: AString::from_str_in(label, alloc),
                    lines,
                });
            }
            RenderBlock::Table(tbl) => {
                out.push(MdBlock::Table(lower_table(tbl, alloc)));
            }
            RenderBlock::Details { summary, open } => {
                out.push(MdBlock::Details { summary: lower_spans(summary, alloc), open });
            }
            RenderBlock::Rule => out.push(MdBlock::Rule),
            RenderBlock::Blank => out.push(MdBlock::Blank),
        }
    }
}

fn lower_style(s: InlineStyle) -> MdStyle {
    if s.kbd {
        MdStyle::Kbd
    } else if s.code {
        MdStyle::Code
    } else if s.link {
        MdStyle::Link
    } else if s.strike {
        MdStyle::Strike
    } else if s.bold && s.italic {
        MdStyle::BoldItalic
    } else if s.bold {
        MdStyle::Bold
    } else if s.italic {
        MdStyle::Italic
    } else {
        MdStyle::Plain
    }
}

fn lower_spans<'a>(raw: AVec<'a, RenderSpan<'a>>, alloc: Alloc<'a>) -> AVec<'a, MdSpan<'a>> {
    let mut out = AVec::with_capacity_in(raw.len(), alloc);
    for sp in raw {
        out.push(MdSpan { text: sp.text, style: lower_style(sp.style), href: sp.href });
    }
    out
}

fn lower_table<'a>(raw: RenderTable<'a>, alloc: Alloc<'a>) -> MdTable<'a> {
    let mut headers = AVec::with_capacity_in(raw.headers.len(), alloc);
    for h in raw.headers {
        headers.push(lower_spans(h, alloc));
    }
    let mut rows = AVec::with_capacity_in(raw.rows.len(), alloc);
    for r in raw.rows {
        let mut row = AVec::with_capacity_in(r.len(), alloc);
        for c in r {
            row.push(lower_spans(c, alloc));
        }
        rows.push(row);
    }
    MdTable { headers, alignments: raw.alignments, rows }
}

fn normalize_code_lang(lang_raw: &str) -> &str {
    match lang_raw {
        "rs" | "rust" => "Rust",
        "ts" | "typescript" => "TypeScript",
        "js" | "javascript" => "JavaScript",
        "py" | "python" => "Python",
        "sh" | "bash" | "zsh" | "shell" => "Shell",
        "toml" => "TOML",
        "json" => "JSON",
        "html" | "htm" => "HTML",
        "nix" => "Nix",
        "sql" => "SQL",
        other => other,
    }
}

fn starts_html_block(trimmed: &str) -> Option<&'static str> {
    if !trimmed.starts_with('<') || trimmed.starts_with("</") || trimmed.starts_with("<!") {
        return None;
    }
    let after = trimmed.get(1..)?;
    let name_len = after
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
        .count();
    if name_len == 0 {
        return None;
    }
    let next_b = after.as_bytes().get(name_len).copied();
    if !matches!(next_b, Some(b'>' | b'/' | b' ' | b'\t')) {
        return None;
    }
    let name = after.get(..name_len)?;
    const BLOCK_TAGS: [&str; 26] = [
        "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "picture", "img", "table", "thead",
        "tbody", "tr", "ul", "ol", "li", "blockquote", "pre", "details", "summary", "hr",
        "center", "figure", "section", "article",
    ];
    for &bt in &BLOCK_TAGS {
        if name.eq_ignore_ascii_case(bt) {
            return Some(bt);
        }
    }
    None
}

fn is_void_block_tag(tag: &str) -> bool {
    tag == "img" || tag == "hr"
}

fn tag_balance_delta(line: &str, tag: &str) -> i32 {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let tlen = tag.len();
    let mut delta: i32 = 0;
    let mut i = 0usize;
    while i < len {
        if bytes.get(i).copied() == Some(b'<') {
            if bytes.get(i.saturating_add(1)).copied() == Some(b'/') {
                let ns = i.saturating_add(2);
                let ne = ns.saturating_add(tlen);
                if line.get(ns..ne).is_some_and(|s| s.eq_ignore_ascii_case(tag))
                    && bytes
                        .get(ne)
                        .copied()
                        .is_some_and(|b| b == b'>' || b.is_ascii_whitespace())
                {
                    delta = delta.saturating_sub(1);
                }
            } else {
                let ns = i.saturating_add(1);
                let ne = ns.saturating_add(tlen);
                if line.get(ns..ne).is_some_and(|s| s.eq_ignore_ascii_case(tag))
                    && bytes
                        .get(ne)
                        .copied()
                        .is_some_and(|b| b == b'>' || b == b'/' || b.is_ascii_whitespace())
                {
                    // Check if self-closing `/>` before next `>`
                    let rest = line.get(ne..).unwrap_or("");
                    let self_close = rest
                        .find('>')
                        .and_then(|gt| rest.as_bytes().get(gt.saturating_sub(1)).copied())
                        == Some(b'/');
                    if !self_close {
                        delta = delta.saturating_add(1);
                    }
                }
            }
        }
        i = i.saturating_add(1);
    }
    delta
}

fn has_closing_tag(line: &str, tag: &str) -> bool {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let tlen = tag.len();
    let mut i = 0usize;
    while i < len {
        if bytes.get(i).copied() == Some(b'<')
            && bytes.get(i.saturating_add(1)).copied() == Some(b'/')
        {
            let ns = i.saturating_add(2);
            let ne = ns.saturating_add(tlen);
            if line.get(ns..ne).is_some_and(|s| s.eq_ignore_ascii_case(tag))
                && bytes
                    .get(ne)
                    .copied()
                    .is_some_and(|b| b == b'>' || b.is_ascii_whitespace())
            {
                return true;
            }
        }
        i = i.saturating_add(1);
    }
    false
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    trimmed.len() >= 3
        && (trimmed.bytes().all(|b| b == b'-' || b == b' ')
            || trimmed.bytes().all(|b| b == b'*' || b == b' ')
            || trimmed.bytes().all(|b| b == b'_' || b == b' '))
        && trimmed.bytes().filter(|&b| b != b' ').count() >= 3
}

fn starts_any_block(trimmed: &str) -> bool {
    if trimmed.starts_with("```")
        || trimmed.starts_with('>')
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || is_horizontal_rule(trimmed)
        || starts_html_block(trimmed).is_some()
        || parse_standalone_md_image(trimmed).is_some()
    {
        return true;
    }
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        return true;
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && digits <= 3 && trimmed.get(digits..).is_some_and(|r| r.starts_with(". "))
}

fn parse_standalone_md_image(trimmed: &str) -> Option<(&str, &str)> {
    let rest = trimmed.strip_prefix("![")?;
    let close_br = rest.find("](")?;
    let alt = rest.get(..close_br)?;
    let after = rest.get(close_br.saturating_add(2)..)?;
    if !after.ends_with(')') {
        return None;
    }
    let url_part = after.get(..after.len().saturating_sub(1))?.trim();
    let src = url_part.split_whitespace().next().unwrap_or("");
    if src.is_empty() { None } else { Some((alt, src)) }
}

fn split_pipe_row<'s, 'a>(line: &'s str, alloc: Alloc<'a>) -> AVec<'a, &'s str> {
    let mut trimmed = line.trim();
    if let Some(s) = trimmed.strip_prefix('|') {
        trimmed = s;
    }
    if let Some(s) = trimmed.strip_suffix('|') {
        trimmed = s;
    }
    let mut out = AVec::new_in(alloc);
    let mut start = 0usize;
    let bytes = trimmed.as_bytes();
    let len = bytes.len();
    let mut in_code = false;
    let mut i = 0usize;
    while i < len {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b == b'`' {
            in_code = !in_code;
        } else if b == b'|' && !in_code {
            out.push(trimmed.get(start..i).unwrap_or("").trim());
            start = i.saturating_add(1);
        }
        i = i.saturating_add(1);
    }
    out.push(trimmed.get(start..).unwrap_or("").trim());
    out
}

fn parse_gfm_table_sep<'a>(line: &str, alloc: Alloc<'a>) -> Option<AVec<'a, TextAlign>> {
    let trimmed = line.trim();
    if !trimmed.contains('-') || !trimmed.contains('|') {
        return None;
    }
    let cells = split_pipe_row(trimmed, alloc);
    if cells.is_empty() {
        return None;
    }
    let mut aligns = AVec::with_capacity_in(cells.len(), alloc);
    for &c in cells.iter() {
        if c.is_empty() {
            return None;
        }
        let left_colon = c.starts_with(':');
        let right_colon = c.ends_with(':');
        let inner = c.trim_matches(':');
        if inner.is_empty() || !inner.bytes().all(|b| b == b'-') {
            return None;
        }
        let align = match (left_colon, right_colon) {
            (true, true) => TextAlign::Center,
            (false, true) => TextAlign::Right,
            _ => TextAlign::Left,
        };
        aligns.push(align);
    }
    Some(aligns)
}

fn push_md_span<'a>(
    spans: &mut AVec<'a, MdSpan<'a>>,
    text: &str,
    style: MdStyle,
    href: Option<&str>,
    alloc: Alloc<'a>,
) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
        && last.href.as_deref() == href
    {
        last.text.push_str(text);
        return;
    }
    spans.push(MdSpan {
        text: AString::from_str_in(text, alloc),
        style,
        href: href.map(|h| AString::from_str_in(h, alloc)),
    });
}

/// Parse inline Markdown (`**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, `[label](url)`)
/// PLUS inline HTML (`<code>`, `<kbd>`, `<b>`/`<strong>`, `<i>`/`<em>`, `<s>`/`<del>`,
/// `<a href="...">`, HTML entities) into styled [`MdSpan`]s.
pub fn parse_inline<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, MdSpan<'a>> {
    let mut out = AVec::new_in(alloc);
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        let b = bytes.get(i).copied().unwrap_or(0);

        // Inline HTML span (`<code>...</code>`, `<kbd>...</kbd>`, `<a href="...">...</a>`, `<b>`, `<i>`, `<s>`)
        if b == b'<'
            && let Some((span_text, style, href, advance)) = try_parse_inline_html(text, i, alloc)
        {
            push_md_span(&mut out, span_text.as_str(), style, href.as_deref(), alloc);
            i = i.saturating_add(advance);
            continue;
        }

        // Inline code `` `...` ``
        if b == b'`'
            && let Some(after) = text.get(i.saturating_add(1)..)
            && let Some(rel) = after.find('`')
        {
            let content = after.get(..rel).unwrap_or("");
            push_md_span(&mut out, content, MdStyle::Code, None, alloc);
            i = i.saturating_add(rel).saturating_add(2);
            continue;
        }

        // Bold `**...**`
        if b == b'*'
            && bytes.get(i.saturating_add(1)).copied() == Some(b'*')
            && let Some(after) = text.get(i.saturating_add(2)..)
            && let Some(rel) = after.find("**")
            && rel > 0
        {
            let inner = decode_entities(after.get(..rel).unwrap_or(""), alloc);
            push_md_span(&mut out, inner.as_str(), MdStyle::Bold, None, alloc);
            i = i.saturating_add(rel).saturating_add(4);
            continue;
        }

        // Strikethrough `~~...~~`
        if b == b'~'
            && bytes.get(i.saturating_add(1)).copied() == Some(b'~')
            && let Some(after) = text.get(i.saturating_add(2)..)
            && let Some(rel) = after.find("~~")
            && rel > 0
        {
            let inner = decode_entities(after.get(..rel).unwrap_or(""), alloc);
            push_md_span(&mut out, inner.as_str(), MdStyle::Strike, None, alloc);
            i = i.saturating_add(rel).saturating_add(4);
            continue;
        }

        // Italic `*...*`
        if b == b'*'
            && let Some(after) = text.get(i.saturating_add(1)..)
            && let Some(rel) = after.find('*')
            && rel > 0
        {
            let inner = decode_entities(after.get(..rel).unwrap_or(""), alloc);
            push_md_span(&mut out, inner.as_str(), MdStyle::Italic, None, alloc);
            i = i.saturating_add(rel).saturating_add(2);
            continue;
        }

        // Markdown link `[label](url)` -> render clean `label` in Link style with `href`!
        if b == b'['
            && let Some(after_open) = text.get(i.saturating_add(1)..)
            && let Some(close_br) = after_open.find("](")
        {
            let url_start = close_br.saturating_add(2);
            if let Some(after_url) = after_open.get(url_start..)
                && let Some(close_par) = after_url.find(')')
            {
                let label = decode_entities(after_open.get(..close_br).unwrap_or(""), alloc);
                let url = after_url.get(..close_par).unwrap_or("");
                push_md_span(&mut out, label.as_str(), MdStyle::Link, Some(url), alloc);
                i = i
                    .saturating_add(1)
                    .saturating_add(url_start)
                    .saturating_add(close_par)
                    .saturating_add(1);
                continue;
            }
        }

        // HTML entity `&...;` inside plain prose
        if b == b'&'
            && let Some(rest) = text.get(i..)
            && let Some(semi) = rest.bytes().take(32).position(|c| c == b';')
        {
            let raw_ent = rest.get(..=semi).unwrap_or("");
            let decoded = decode_entities(raw_ent, alloc);
            push_md_span(&mut out, decoded.as_str(), MdStyle::Plain, None, alloc);
            i = i.saturating_add(semi).saturating_add(1);
            continue;
        }

        let ch_len = text
            .get(i..)
            .and_then(|s| s.chars().next())
            .map_or(1, char::len_utf8);
        let piece = text.get(i..i.saturating_add(ch_len)).unwrap_or("");
        push_md_span(&mut out, piece, MdStyle::Plain, None, alloc);
        i = i.saturating_add(ch_len);
    }
    out
}

fn try_parse_inline_html<'a>(
    text: &str,
    at: usize,
    alloc: Alloc<'a>,
) -> Option<(AString<'a>, MdStyle, Option<AString<'a>>, usize)> {
    let rest = text.get(at..)?;
    if !rest.starts_with('<') || rest.starts_with("</") || rest.starts_with("<!") {
        return None;
    }
    let gt = rest.find('>')?;
    let open_inner = rest.get(1..gt)?.trim();
    let tag_len = open_inner
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    if tag_len == 0 {
        return None;
    }
    let tag = open_inner.get(..tag_len)?;
    let style = if tag.eq_ignore_ascii_case("code") || tag.eq_ignore_ascii_case("samp") {
        MdStyle::Code
    } else if tag.eq_ignore_ascii_case("kbd") {
        MdStyle::Kbd
    } else if tag.eq_ignore_ascii_case("b") || tag.eq_ignore_ascii_case("strong") {
        MdStyle::Bold
    } else if tag.eq_ignore_ascii_case("i") || tag.eq_ignore_ascii_case("em") {
        MdStyle::Italic
    } else if tag.eq_ignore_ascii_case("s")
        || tag.eq_ignore_ascii_case("del")
        || tag.eq_ignore_ascii_case("strike")
    {
        MdStyle::Strike
    } else if tag.eq_ignore_ascii_case("a") {
        MdStyle::Link
    } else if tag.eq_ignore_ascii_case("span")
        || tag.eq_ignore_ascii_case("sub")
        || tag.eq_ignore_ascii_case("sup")
        || tag.eq_ignore_ascii_case("small")
    {
        MdStyle::Plain
    } else {
        return None;
    };

    let after_open = rest.get(gt.saturating_add(1)..)?;
    // Find matching `</tag>` (case-insensitive).
    let close_pos = find_inline_close(after_open, tag)?;
    let body = after_open.get(..close_pos)?;
    let close_end_rel = after_open.get(close_pos..)?.find('>')?;
    let total_advance = gt
        .saturating_add(1)
        .saturating_add(close_pos)
        .saturating_add(close_end_rel)
        .saturating_add(1);

    let href = if style == MdStyle::Link {
        extract_href(open_inner, alloc)
    } else {
        None
    };
    let decoded = decode_entities(body, alloc);
    Some((decoded, style, href, total_advance))
}

fn find_inline_close(s: &str, tag: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let tlen = tag.len();
    let mut i = 0usize;
    while i < len {
        if bytes.get(i).copied() == Some(b'<')
            && bytes.get(i.saturating_add(1)).copied() == Some(b'/')
        {
            let ns = i.saturating_add(2);
            let ne = ns.saturating_add(tlen);
            if s.get(ns..ne).is_some_and(|w| w.eq_ignore_ascii_case(tag))
                && bytes
                    .get(ne)
                    .copied()
                    .is_some_and(|b| b == b'>' || b.is_ascii_whitespace())
            {
                return Some(i);
            }
        }
        i = i.saturating_add(1);
    }
    None
}

fn extract_href<'a>(open_inner: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let lower_pos = find_ascii_ci(open_inner, "href=")?;
    let after = open_inner.get(lower_pos.saturating_add(5)..)?.trim_start();
    let val = if let Some(q) = after.strip_prefix('"') {
        q.split('"').next()?
    } else if let Some(q) = after.strip_prefix('\'') {
        q.split('\'').next()?
    } else {
        after.split_whitespace().next()?
    };
    Some(decode_entities(val, alloc))
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    let limit = h.len().saturating_sub(n.len());
    let mut i = 0usize;
    while i <= limit {
        let mut ok = true;
        let mut j = 0usize;
        while j < n.len() {
            if !h
                .get(i.saturating_add(j))
                .copied()
                .unwrap_or(0)
                .eq_ignore_ascii_case(&n.get(j).copied().unwrap_or(0))
            {
                ok = false;
                break;
            }
            j = j.saturating_add(1);
        }
        if ok {
            return Some(i);
        }
        i = i.saturating_add(1);
    }
    None
}
