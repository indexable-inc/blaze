//! Pure `#![no_std]` Markdown renderer model for `blaze-preview`: parses headings, paragraphs,
//! bullet/numbered lists, fenced code blocks (syntax-highlighted!), blockquotes, horizontal rules,
//! and inline **bold**, *italic*, and `` `code` `` spans for direct GPU rendering.

use blaze_own::{AString, AVec, Alloc};

use crate::highlight::{CodeLine, highlight_code};

/// Inline text style inside a rendered Markdown paragraph, heading, or list item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdStyle {
    Plain,
    Bold,
    Italic,
    Code,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdSpan<'a> {
    pub text: AString<'a>,
    pub style: MdStyle,
}

/// One visual block in a rendered Markdown document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdBlock<'a> {
    Heading {
        level: u8,
        spans: AVec<'a, MdSpan<'a>>,
    },
    Paragraph {
        spans: AVec<'a, MdSpan<'a>>,
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
    Rule,
    Blank,
}

/// Parse up to `max_blocks` rendered Markdown blocks from `src` into `alloc`.
pub fn parse_markdown<'a>(src: &str, max_blocks: usize, alloc: Alloc<'a>) -> AVec<'a, MdBlock<'a>> {
    let mut blocks = AVec::new_in(alloc);
    let mut lines = src.lines().peekable();

    while let Some(raw) = lines.next() {
        if blocks.len() >= max_blocks {
            break;
        }
        let trimmed = raw.trim();

        // Skip YAML front matter (`---` at start of file).
        if blocks.is_empty() && trimmed == "---" {
            for fm in lines.by_ref() {
                if fm.trim() == "---" || fm.trim() == "..." {
                    break;
                }
            }
            continue;
        }

        if trimmed.is_empty() {
            if !matches!(blocks.last(), None | Some(MdBlock::Blank)) {
                blocks.push(MdBlock::Blank);
            }
            continue;
        }

        // Fenced code block (` ```lang ... ``` `)
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang_raw = rest.trim();
            let lang_label = match lang_raw {
                "rs" | "rust" => "Rust",
                "ts" | "typescript" => "TypeScript",
                "js" | "javascript" => "JavaScript",
                "py" | "python" => "Python",
                "sh" | "bash" | "zsh" => "Shell",
                "toml" => "TOML",
                "json" => "JSON",
                "nix" => "Nix",
                "sql" => "SQL",
                other => other,
            };
            let mut body = AString::new_in(alloc);
            let mut count = 0usize;
            for code_line in lines.by_ref() {
                if code_line.trim().starts_with("```") {
                    break;
                }
                if count < 24 {
                    if count > 0 {
                        body.push('\n');
                    }
                    body.push_str(code_line);
                    count += 1;
                }
            }
            let hl = highlight_code(body.as_str(), lang_label, 24, alloc);
            blocks.push(MdBlock::CodeFence {
                lang: AString::from_str_in(lang_label, alloc),
                lines: hl,
            });
            continue;
        }

        // Horizontal rule (`---`, `***`, `___`)
        if trimmed.len() >= 3
            && (trimmed.bytes().all(|b| b == b'-' || b == b' ')
                || trimmed.bytes().all(|b| b == b'*' || b == b' ')
                || trimmed.bytes().all(|b| b == b'_' || b == b' '))
            && trimmed.bytes().filter(|&b| b != b' ').count() >= 3
        {
            blocks.push(MdBlock::Rule);
            continue;
        }

        // ATX Heading (`#`..`######`)
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes)
            && trimmed.as_bytes().get(hashes) == Some(&b' ')
        {
            let content = trimmed[hashes + 1..].trim().trim_end_matches('#').trim();
            blocks.push(MdBlock::Heading {
                level: hashes as u8,
                spans: parse_inline(content, alloc),
            });
            continue;
        }

        // Blockquote (`> ...`)
        if let Some(rest) = trimmed.strip_prefix('>') {
            let content = rest.trim_start();
            blocks.push(MdBlock::Quote { spans: parse_inline(content, alloc) });
            continue;
        }

        // Unordered bullet (`- `, `* `, `+ `)
        let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            blocks.push(MdBlock::Bullet {
                indent: (leading_spaces / 2).min(4) as u8,
                marker: AString::from_str_in("•", alloc),
                spans: parse_inline(rest.trim(), alloc),
            });
            continue;
        }

        // Ordered list (`1. `, `2. `)
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && digits <= 3
            && trimmed[digits..].starts_with(". ")
        {
            let marker = &trimmed[..=digits];
            let rest = trimmed[digits + 2..].trim();
            blocks.push(MdBlock::Bullet {
                indent: (leading_spaces / 2).min(4) as u8,
                marker: AString::from_str_in(marker, alloc),
                spans: parse_inline(rest, alloc),
            });
            continue;
        }

        blocks.push(MdBlock::Paragraph { spans: parse_inline(trimmed, alloc) });
    }

    blocks
}

fn push_md_span<'a>(spans: &mut AVec<'a, MdSpan<'a>>, text: &str, style: MdStyle, alloc: Alloc<'a>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.text.push_str(text);
        return;
    }
    spans.push(MdSpan { text: AString::from_str_in(text, alloc), style });
}

/// Parse inline Markdown (`**bold**`, `*italic*`, `` `code` ``, `[label](url)`).
pub fn parse_inline<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, MdSpan<'a>> {
    let mut out = AVec::new_in(alloc);
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        // Inline code `` `...` ``
        if bytes[i] == b'`' {
            if let Some(rel) = text[i + 1..].find('`') {
                push_md_span(&mut out, &text[i + 1..i + 1 + rel], MdStyle::Code, alloc);
                i += rel + 2;
                continue;
            }
        }
        // Bold `**...**`
        if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(rel) = text[i + 2..].find("**") {
                push_md_span(&mut out, &text[i + 2..i + 2 + rel], MdStyle::Bold, alloc);
                i += rel + 4;
                continue;
            }
        }
        // Italic `*...*`
        if bytes[i] == b'*' {
            if let Some(rel) = text[i + 1..].find('*')
                && rel > 0
            {
                push_md_span(&mut out, &text[i + 1..i + 1 + rel], MdStyle::Italic, alloc);
                i += rel + 2;
                continue;
            }
        }
        // Markdown link `[label](url)` -> render clean `label` in Link style!
        if bytes[i] == b'[' {
            if let Some(close_br) = text[i + 1..].find("](") {
                let after = i + 1 + close_br + 2;
                if let Some(close_par) = text[after..].find(')') {
                    push_md_span(&mut out, &text[i + 1..i + 1 + close_br], MdStyle::Link, alloc);
                    i = after + close_par + 1;
                    continue;
                }
            }
        }
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        push_md_span(&mut out, &text[i..i + ch_len], MdStyle::Plain, alloc);
        i += ch_len;
    }
    out
}
