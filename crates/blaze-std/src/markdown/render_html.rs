//! Iterative, XSS-safe HTML renderer for [`Document`].

use core::alloc::Allocator;
use core::fmt;

use crate::markdown::{Block, Document, Inline};
use crate::table::Align;

/// Write `text` to `out` escaping `&`, `<`, `>`, and `"` for safe HTML text and attribute values.
fn write_html_escaped<W: fmt::Write>(
    text: &str,
    out: &mut W,
    plant_skip_lt_escape: bool,
) -> fmt::Result {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' if !plant_skip_lt_escape => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            _ => continue,
        };
        if i > start
            && let Some(chunk) = text.get(start..i)
        {
            out.write_str(chunk)?;
        }
        out.write_str(replacement)?;
        start = i + 1;
    }
    if start < text.len()
        && let Some(tail) = text.get(start..)
    {
        out.write_str(tail)?;
    }
    Ok(())
}

fn render_inlines_html<W: fmt::Write>(
    inlines: &[Inline<'_>],
    out: &mut W,
    plant_skip_lt_escape: bool,
) -> fmt::Result {
    for &inline in inlines {
        match inline {
            Inline::Text(s) => write_html_escaped(s, out, plant_skip_lt_escape)?,
            Inline::Code(s) => {
                out.write_str("<code>")?;
                write_html_escaped(s, out, plant_skip_lt_escape)?;
                out.write_str("</code>")?;
            }
            Inline::Bold(s) => {
                out.write_str("<strong>")?;
                write_html_escaped(s, out, plant_skip_lt_escape)?;
                out.write_str("</strong>")?;
            }
            Inline::Italic(s) => {
                out.write_str("<em>")?;
                write_html_escaped(s, out, plant_skip_lt_escape)?;
                out.write_str("</em>")?;
            }
            Inline::Strikethrough(s) => {
                out.write_str("<del>")?;
                write_html_escaped(s, out, plant_skip_lt_escape)?;
                out.write_str("</del>")?;
            }
            Inline::Link { text, url } => {
                out.write_str("<a href=\"")?;
                write_html_escaped(url, out, plant_skip_lt_escape)?;
                out.write_str("\">")?;
                write_html_escaped(text, out, plant_skip_lt_escape)?;
                out.write_str("</a>")?;
            }
            Inline::Image { alt, url } => {
                out.write_str("<img src=\"")?;
                write_html_escaped(url, out, plant_skip_lt_escape)?;
                out.write_str("\" alt=\"")?;
                write_html_escaped(alt, out, plant_skip_lt_escape)?;
                out.write_str("\" />")?;
            }
            Inline::SoftBreak => out.write_char('\n')?,
            Inline::HardBreak => out.write_str("<br />\n")?,
        }
    }
    Ok(())
}

fn write_align_attr<W: fmt::Write>(align: Align, out: &mut W) -> fmt::Result {
    match align {
        Align::Left => Ok(()),
        Align::Center => out.write_str(" align=\"center\""),
        Align::Right => out.write_str(" align=\"right\""),
    }
}

impl<A: Allocator> Document<'_, A> {
    /// Render this document as XSS-safe HTML into `out`.
    pub fn render_html_into<W: fmt::Write>(&self, out: &mut W) -> fmt::Result {
        self.render_html_into_planted(out, false)
    }

    pub(super) fn render_html_into_planted<W: fmt::Write>(
        &self,
        out: &mut W,
        plant_skip_lt_escape: bool,
    ) -> fmt::Result {
        for block in self.blocks.as_slice() {
            match block {
                Block::Heading { level, inlines } => {
                    let lvl = (*level).clamp(1, 6);
                    write!(out, "<h{lvl}>")?;
                    render_inlines_html(
                        self.inlines_for(inlines.start..inlines.end),
                        out,
                        plant_skip_lt_escape,
                    )?;
                    writeln!(out, "</h{lvl}>")?;
                }
                Block::Paragraph { inlines } => {
                    out.write_str("<p>")?;
                    render_inlines_html(
                        self.inlines_for(inlines.start..inlines.end),
                        out,
                        plant_skip_lt_escape,
                    )?;
                    out.write_str("</p>\n")?;
                }
                Block::CodeBlock { lang, code } => {
                    if let Some(language) = lang {
                        out.write_str("<pre><code class=\"language-")?;
                        write_html_escaped(language, out, plant_skip_lt_escape)?;
                        out.write_str("\">")?;
                    } else {
                        out.write_str("<pre><code>")?;
                    }
                    write_html_escaped(code, out, plant_skip_lt_escape)?;
                    out.write_str("</code></pre>\n")?;
                }
                Block::BlockQuote { lines } => {
                    out.write_str("<blockquote><p>")?;
                    for (idx, &line) in lines.as_slice().iter().enumerate() {
                        if idx > 0 {
                            out.write_char('\n')?;
                        }
                        write_html_escaped(line, out, plant_skip_lt_escape)?;
                    }
                    out.write_str("</p></blockquote>\n")?;
                }
                Block::List { ordered, start, items } => {
                    if *ordered {
                        if *start == 1 {
                            out.write_str("<ol>\n")?;
                        } else {
                            writeln!(out, "<ol start=\"{start}\">")?;
                        }
                    } else {
                        out.write_str("<ul>\n")?;
                    }
                    for item in items.as_slice() {
                        out.write_str("<li>")?;
                        render_inlines_html(
                            self.inlines_for(item.start..item.end),
                            out,
                            plant_skip_lt_escape,
                        )?;
                        out.write_str("</li>\n")?;
                    }
                    if *ordered {
                        out.write_str("</ol>\n")?;
                    } else {
                        out.write_str("</ul>\n")?;
                    }
                }
                Block::ThematicBreak => {
                    out.write_str("<hr />\n")?;
                }
                Block::Table { headers, aligns, rows } => {
                    out.write_str("<table>\n<thead>\n<tr>\n")?;
                    for (col, h_range) in headers.as_slice().iter().enumerate() {
                        let align = aligns.as_slice().get(col).copied().unwrap_or(Align::Left);
                        out.write_str("<th")?;
                        write_align_attr(align, out)?;
                        out.write_char('>')?;
                        render_inlines_html(
                            self.inlines_for(h_range.start..h_range.end),
                            out,
                            plant_skip_lt_escape,
                        )?;
                        out.write_str("</th>\n")?;
                    }
                    out.write_str("</tr>\n</thead>\n")?;
                    if !rows.is_empty() {
                        out.write_str("<tbody>\n")?;
                        for row in rows.as_slice() {
                            out.write_str("<tr>\n")?;
                            for (col, cell_range) in row.as_slice().iter().enumerate() {
                                let align =
                                    aligns.as_slice().get(col).copied().unwrap_or(Align::Left);
                                out.write_str("<td")?;
                                write_align_attr(align, out)?;
                                out.write_char('>')?;
                                render_inlines_html(
                                    self.inlines_for(cell_range.start..cell_range.end),
                                    out,
                                    plant_skip_lt_escape,
                                )?;
                                out.write_str("</td>\n")?;
                            }
                            out.write_str("</tr>\n")?;
                        }
                        out.write_str("</tbody>\n")?;
                    }
                    out.write_str("</table>\n")?;
                }
            }
        }
        Ok(())
    }
}
