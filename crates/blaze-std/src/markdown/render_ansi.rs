//! Iterative ANSI terminal renderer for [`Document`], built on [`crate::ansi`] and
//! [`crate::table`].

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

use crate::ansi::{Color, Style};
use crate::markdown::{Block, Document, Inline, TryStringWriter};
use crate::table::{BoxStyle, TableOpts, render_table_into};

/// Failure during ANSI rendering (either downstream `fmt::Write` error or temp-buffer OOM).
pub(super) enum AnsiRenderError {
    Fmt(fmt::Error),
    Alloc(AllocFailure),
}

impl From<fmt::Error> for AnsiRenderError {
    #[inline]
    fn from(err: fmt::Error) -> Self {
        Self::Fmt(err)
    }
}

impl From<AllocFailure> for AnsiRenderError {
    #[inline]
    fn from(why: AllocFailure) -> Self {
        Self::Alloc(why)
    }
}

fn render_inlines_ansi<W: fmt::Write>(
    inlines: &[Inline<'_>],
    base_style: Style,
    out: &mut W,
) -> fmt::Result {
    for &inline in inlines {
        match inline {
            Inline::Text(s) => write!(out, "{}", base_style.paint(s))?,
            Inline::Code(s) => write!(out, "{}", base_style.fg(Color::Yellow).paint(s))?,
            Inline::Bold(s) => write!(out, "{}", base_style.bold().paint(s))?,
            Inline::Italic(s) => write!(out, "{}", base_style.italic().paint(s))?,
            Inline::Strikethrough(s) => write!(out, "{}", base_style.strikethrough().paint(s))?,
            Inline::Link { text, url } => {
                write!(
                    out,
                    "{} ({})",
                    base_style.underline().fg(Color::Blue).paint(text),
                    Style::new().dim().paint(url),
                )?;
            }
            Inline::Image { alt, url } => {
                let dim = Style::new().dim();
                if url.is_empty() {
                    write!(out, "{}", dim.paint(alt))?;
                } else {
                    write!(out, "{} ({})", dim.paint(alt), dim.paint(url))?;
                }
            }
            Inline::SoftBreak | Inline::HardBreak => out.write_char('\n')?,
        }
    }
    Ok(())
}

fn render_cell_in<A: Allocator + Copy>(
    inlines: &[Inline<'_>],
    base_style: Style,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut cell = String::new_in(alloc);
    let mut writer = TryStringWriter { out: &mut cell, err: None };
    if render_inlines_ansi(inlines, base_style, &mut writer).is_err()
        && let Some(why) = writer.err
    {
        return Err(why);
    }
    Ok(cell)
}

impl<A: Allocator> Document<'_, A> {
    /// Render this document as ANSI-styled terminal text into `out`.
    pub fn render_ansi_into<W: fmt::Write>(&self, out: &mut W) -> fmt::Result {
        match self.render_ansi_into_fallible(out) {
            Ok(()) => Ok(()),
            Err(AnsiRenderError::Fmt(err)) => Err(err),
            Err(AnsiRenderError::Alloc(_)) => Err(fmt::Error),
        }
    }

    pub(super) fn render_ansi_into_fallible<W: fmt::Write>(
        &self,
        out: &mut W,
    ) -> Result<(), AnsiRenderError> {
        let heading_style = Style::new().bold().fg(Color::Cyan);
        let dim_style = Style::new().dim();
        let quote_bar = Style::new().dim().fg(Color::BrightBlack);
        let quote_text = Style::new().italic();
        let bullet_style = Style::new().fg(Color::Cyan);
        let header_cell_style = Style::new().bold();

        for (block_idx, block) in self.blocks.as_slice().iter().enumerate() {
            if block_idx > 0 {
                out.write_char('\n')?;
            }
            match block {
                Block::Heading { level, inlines } => {
                    let lvl = usize::from((*level).clamp(1, 6));
                    for _ in 0..lvl {
                        write!(out, "{}", heading_style.paint('#'))?;
                    }
                    out.write_char(' ')?;
                    render_inlines_ansi(
                        self.inlines_for(inlines.start..inlines.end),
                        heading_style,
                        out,
                    )?;
                    out.write_char('\n')?;
                }
                Block::Paragraph { inlines } => {
                    render_inlines_ansi(
                        self.inlines_for(inlines.start..inlines.end),
                        Style::PLAIN,
                        out,
                    )?;
                    out.write_char('\n')?;
                }
                Block::CodeBlock { lang, code } => {
                    if let Some(language) = lang {
                        writeln!(out, "  {}", dim_style.paint(language))?;
                    }
                    if !code.is_empty() {
                        for line in code.split('\n') {
                            writeln!(out, "    {}", dim_style.paint(line))?;
                        }
                    }
                }
                Block::BlockQuote { lines } => {
                    for &line in lines.as_slice() {
                        writeln!(out, "{} {}", quote_bar.paint('│'), quote_text.paint(line))?;
                    }
                }
                Block::List { ordered, start, items } => {
                    for (idx, item) in items.as_slice().iter().enumerate() {
                        if *ordered {
                            let offset = u64::try_from(idx).unwrap_or(u64::MAX);
                            let num = u64::from(*start).saturating_add(offset);
                            write!(out, "  {}. ", bullet_style.paint(num))?;
                        } else {
                            write!(out, "  {} ", bullet_style.paint('•'))?;
                        }
                        render_inlines_ansi(
                            self.inlines_for(item.start..item.end),
                            Style::PLAIN,
                            out,
                        )?;
                        out.write_char('\n')?;
                    }
                }
                Block::ThematicBreak => {
                    writeln!(out, "{}", dim_style.paint("────────────────────────────────"))?;
                }
                Block::Table { headers, aligns, rows } => {
                    let alloc = self.blocks.allocator();
                    let mut rendered_headers: Vec<String<&A>, &A> =
                        Vec::try_with_capacity_in(headers.len(), alloc)?;
                    for h_range in headers.as_slice() {
                        let cell = render_cell_in(
                            self.inlines_for(h_range.start..h_range.end),
                            header_cell_style,
                            alloc,
                        )?;
                        rendered_headers.try_push(cell).map_err(|r| AnsiRenderError::Alloc(r.why))?;
                    }
                    let mut rendered_rows: Vec<Vec<String<&A>, &A>, &A> =
                        Vec::try_with_capacity_in(rows.len(), alloc)?;
                    for row in rows.as_slice() {
                        let mut rendered_row: Vec<String<&A>, &A> =
                            Vec::try_with_capacity_in(row.len(), alloc)?;
                        for c_range in row.as_slice() {
                            let cell = render_cell_in(
                                self.inlines_for(c_range.start..c_range.end),
                                Style::PLAIN,
                                alloc,
                            )?;
                            rendered_row
                                .try_push(cell)
                                .map_err(|r| AnsiRenderError::Alloc(r.why))?;
                        }
                        rendered_rows
                            .try_push(rendered_row)
                            .map_err(|r| AnsiRenderError::Alloc(r.why))?;
                    }
                    render_table_into(
                        rendered_headers.as_slice(),
                        rendered_rows.as_slice(),
                        aligns.as_slice(),
                        TableOpts::new(BoxStyle::Rounded),
                        out,
                    )?;
                    out.write_char('\n')?;
                }
            }
        }
        Ok(())
    }
}
