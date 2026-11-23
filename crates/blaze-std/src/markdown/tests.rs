//! Unit, property, and planted-bug tests for [`crate::markdown`].

use blaze_own::{String as OwnString, format_in, testing::heap};
use blaze_sim::{Broke, Config};

use crate::ansi::{has_ansi, strip_in, visible_width};
use crate::markdown::{
    Block, Inline, MarkdownError, MarkdownLimits, parse_in, to_ansi_in, to_html_in,
};
use crate::table::Align;

#[test]
fn parses_headings_paragraphs_breaks_and_rules() {
    let h = heap();
    let src = "\
# Heading One
### Heading Three ###

First line
second line  
third line\\
fourth line

---

***
";
    let doc = parse_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("parse");
    assert_eq!(doc.blocks.len(), 5);
    assert!(matches!(doc.blocks.as_slice().first(), Some(Block::Heading { level: 1, .. })));
    assert!(matches!(doc.blocks.as_slice().get(1), Some(Block::Heading { level: 3, .. })));
    assert!(matches!(doc.blocks.as_slice().get(3), Some(Block::ThematicBreak)));
    assert!(matches!(doc.blocks.as_slice().get(4), Some(Block::ThematicBreak)));

    let html = to_html_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("html");
    assert!(html.as_str().contains("<h1>Heading One</h1>\n"));
    assert!(html.as_str().contains("<h3>Heading Three</h3>\n"));
    assert!(html.as_str().contains("<p>First line\nsecond line<br />\nthird line<br />\nfourth line</p>\n"));
    assert!(html.as_str().contains("<hr />\n"));
}

#[test]
fn parses_code_blocks_blockquotes_and_lists() {
    let h = heap();
    let src = "\
```rust
fn add(a: u32, b: u32) -> u32 { a + b }
```

~~~
plain tilde fence
~~~

> Quote line one
> Quote line two

- apple
- banana

3. third
4. fourth
";
    let doc = parse_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("parse");
    assert_eq!(doc.blocks.len(), 5);
    assert!(matches!(
        doc.blocks.as_slice().first(),
        Some(Block::CodeBlock { lang: Some("rust"), code: "fn add(a: u32, b: u32) -> u32 { a + b }" })
    ));
    assert!(matches!(
        doc.blocks.as_slice().get(1),
        Some(Block::CodeBlock { lang: None, code: "plain tilde fence" })
    ));
    assert!(matches!(
        doc.blocks.as_slice().get(3),
        Some(Block::List { ordered: false, start: 1, items }) if items.len() == 2
    ));
    assert!(matches!(
        doc.blocks.as_slice().get(4),
        Some(Block::List { ordered: true, start: 3, items }) if items.len() == 2
    ));

    let html = to_html_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("html");
    assert!(html.as_str().contains("<pre><code class=\"language-rust\">fn add(a: u32, b: u32) -&gt; u32 { a + b }</code></pre>\n"));
    assert!(html.as_str().contains("<blockquote><p>Quote line one\nQuote line two</p></blockquote>\n"));
    assert!(html.as_str().contains("<ul>\n<li>apple</li>\n<li>banana</li>\n</ul>\n"));
    assert!(html.as_str().contains("<ol start=\"3\">\n<li>third</li>\n<li>fourth</li>\n</ol>\n"));

    let ansi = to_ansi_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("ansi");
    assert!(has_ansi(ansi.as_str()));
    let plain = strip_in(ansi.as_str(), h.alloc()).expect("strip");
    assert!(plain.as_str().contains("fn add(a: u32, b: u32) -> u32 { a + b }"));
    assert!(plain.as_str().contains("│ Quote line one"));
    assert!(plain.as_str().contains("• apple"));
    assert!(plain.as_str().contains("3. third"));
    assert!(plain.as_str().contains("4. fourth"));
}

#[test]
fn parses_inlines_and_preserves_snake_case() {
    let h = heap();
    let src = "See **bold**, *italic*, _under_, ~~gone~~, `code`, foo_bar_baz, [Blaze](https://ix.dev/rust_(v2)), and ![logo](https://ix.dev/logo.png).";
    let doc = parse_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("parse");
    assert_eq!(doc.blocks.len(), 1);
    let inlines = match doc.blocks.as_slice().first() {
        Some(Block::Paragraph { inlines }) => doc.inlines_for(inlines.start..inlines.end),
        other => panic!("expected paragraph, got {other:?}"),
    };
    assert!(inlines.contains(&Inline::Bold("bold")));
    assert!(inlines.contains(&Inline::Italic("italic")));
    assert!(inlines.contains(&Inline::Italic("under")));
    assert!(inlines.contains(&Inline::Strikethrough("gone")));
    assert!(inlines.contains(&Inline::Code("code")));
    assert!(inlines.contains(&Inline::Link { text: "Blaze", url: "https://ix.dev/rust_(v2)" }));
    assert!(inlines.contains(&Inline::Image { alt: "logo", url: "https://ix.dev/logo.png" }));

    let html = to_html_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("html");
    assert!(html.as_str().contains("<strong>bold</strong>"));
    assert!(html.as_str().contains("<em>italic</em>"));
    assert!(html.as_str().contains("<em>under</em>"));
    assert!(html.as_str().contains("<del>gone</del>"));
    assert!(html.as_str().contains("<code>code</code>"));
    assert!(html.as_str().contains("foo_bar_baz"));
    assert!(html.as_str().contains("<a href=\"https://ix.dev/rust_(v2)\">Blaze</a>"));
    assert!(html.as_str().contains("<img src=\"https://ix.dev/logo.png\" alt=\"logo\" />"));

    let ansi = to_ansi_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("ansi");
    assert!(has_ansi(ansi.as_str()));
    let stripped = strip_in(ansi.as_str(), h.alloc()).expect("strip");
    assert!(!has_ansi(stripped.as_str()));
    assert!(stripped.as_str().contains("bold"));
    assert!(stripped.as_str().contains("foo_bar_baz"));
}

#[test]
fn parses_and_renders_gfm_table_html_and_ansi() {
    let h = heap();
    let src = "\
| Crate | Score | Note |
| :--- | :---: | ---: |
| **bstd** | 100 | `fast` |
| blaze | 99 | ok |
";
    let doc = parse_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("parse");
    assert_eq!(doc.blocks.len(), 1);
    match doc.blocks.as_slice().first() {
        Some(Block::Table { headers, aligns, rows }) => {
            assert_eq!(headers.len(), 3);
            assert_eq!(aligns.as_slice(), &[Align::Left, Align::Center, Align::Right]);
            assert_eq!(rows.len(), 2);
        }
        other => panic!("expected table, got {other:?}"),
    }

    let html = to_html_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("html");
    assert!(html.as_str().contains("<table>\n<thead>\n<tr>\n<th>Crate</th>\n<th align=\"center\">Score</th>\n<th align=\"right\">Note</th>\n</tr>\n</thead>\n"));
    assert!(html.as_str().contains("<td><strong>bstd</strong></td>\n"));
    assert!(html.as_str().contains("<td align=\"right\"><code>fast</code></td>\n"));

    let ansi = to_ansi_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("ansi");
    assert!(has_ansi(ansi.as_str()));
    let table_lines = ansi.as_str().trim_end_matches('\n');
    let mut iter = table_lines.split('\n');
    let first_w = visible_width(iter.next().expect("top border"));
    assert!(first_w > 0);
    for line in iter {
        assert_eq!(visible_width(line), first_w, "uneven ANSI table row: {line:?}");
    }
}

#[test]
fn escapes_xss_and_html_special_chars() {
    let h = heap();
    let src = "\
# <script>alert(\"x\")</script> & more

Paragraph with <script>evil()</script> & \"quotes\".

[click <script>](https://example.com/?a=1&b=\"<script>\")

![<script>](img\"<script>.png)

> <script>quote</script>

```<script>
<script>code</script>
```
";
    let html = to_html_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("html");
    assert!(!html.as_str().contains("<script>"));
    assert!(!html.as_str().contains("</script>"));
    assert!(html.as_str().contains("&lt;script&gt;"));
    assert!(html.as_str().contains("&amp;"));
    assert!(html.as_str().contains("&quot;"));
}

#[test]
fn enforces_markdown_limits() {
    let h = heap();
    let tiny_bytes = MarkdownLimits { most_bytes: 5, most_blocks: 100, most_inlines: 100 };
    assert!(matches!(
        parse_in("123456", tiny_bytes, h.alloc()),
        Err(MarkdownError::InputTooLarge { bytes: 6, limit: 5 })
    ));

    let tiny_blocks = MarkdownLimits { most_bytes: 1024, most_blocks: 1, most_inlines: 100 };
    assert!(matches!(
        parse_in("# A\n\n# B\n", tiny_blocks, h.alloc()),
        Err(MarkdownError::TooManyBlocks { limit: 1 })
    ));

    let tiny_inlines = MarkdownLimits { most_bytes: 1024, most_blocks: 100, most_inlines: 2 };
    assert!(matches!(
        parse_in("a **b** *c* `d`", tiny_inlines, h.alloc()),
        Err(MarkdownError::TooManyInlines { limit: 2 })
    ));
}

/// The invariant: hostile markdown respects depth/size limits, escapes HTML script tags, and strips ANSI escapes.
#[test]
fn property_hostile_markdown_obeys_limits_escapes_script_and_strips_ansi() {
    let h = heap();
    const TOKENS: &[&str] = &[
        "# ",
        "### ",
        "```rust\n",
        "~~~\n",
        "> ",
        "- ",
        "1. ",
        "---",
        "| a | b |\n| :--- | ---: |\n| 1 | 2 |\n",
        "**bold**",
        "*em*",
        "~~del~~",
        "`code`",
        "[link](https://ix.dev)",
        "![alt](img.png)",
        "<script>alert(1)</script>",
        "[[[[",
        "****",
        "````",
        "\\",
        "\n",
        "\n\n",
        "  \n",
        "日本",
        "plain text ",
    ];

    blaze_sim::check(Config::cases(256), |tape| {
        let count = usize::try_from(tape.int_in(0..=16, "count")).unwrap_or(0);
        let mut src = OwnString::new_in(h.alloc());
        for _ in 0..count {
            let idx =
                usize::try_from(tape.below(u64::try_from(TOKENS.len()).unwrap_or(1), "tok"))
                    .unwrap_or(0);
            src.push_str(TOKENS[idx]);
        }
        if tape.coin("inject_script") {
            src.push_str("\n<script>xss()</script>\n");
        }

        let limits = MarkdownLimits { most_bytes: 4096, most_blocks: 32, most_inlines: 128 };
        match parse_in(src.as_str(), limits, h.alloc()) {
            Ok(doc) => {
                if doc.blocks.len() > limits.most_blocks {
                    return Err(Broke::Oracle(
                        format_in!(
                            h.alloc(),
                            "blocks {} > limit {}",
                            doc.blocks.len(),
                            limits.most_blocks
                        )
                        .as_str()
                        .into(),
                    ));
                }
                if doc.inlines.len() > limits.most_inlines {
                    return Err(Broke::Oracle(
                        format_in!(
                            h.alloc(),
                            "inlines {} > limit {}",
                            doc.inlines.len(),
                            limits.most_inlines
                        )
                        .as_str()
                        .into(),
                    ));
                }
                let mut html = OwnString::new_in(h.alloc());
                doc.render_html_into(&mut html).expect("render html");
                if html.as_str().contains("<script") {
                    return Err(Broke::Oracle(
                        format_in!(h.alloc(), "unescaped <script in HTML: {:?}", html.as_str())
                            .as_str()
                            .into(),
                    ));
                }
                let mut ansi = OwnString::new_in(h.alloc());
                doc.render_ansi_into(&mut ansi).expect("render ansi");
                let stripped = strip_in(ansi.as_str(), h.alloc()).expect("strip");
                if has_ansi(stripped.as_str()) {
                    return Err(Broke::Oracle(
                        format_in!(h.alloc(), "ANSI remained after strip: {:?}", stripped.as_str())
                            .as_str()
                            .into(),
                    ));
                }
            }
            Err(
                MarkdownError::InputTooLarge { .. }
                | MarkdownError::TooManyBlocks { .. }
                | MarkdownError::TooManyInlines { .. },
            ) => {}
            Err(MarkdownError::Alloc(why)) => {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "unexpected alloc error: {why}").as_str().into(),
                ));
            }
        }
        Ok(())
    });
}

#[test]
fn the_oracle_catches_unescaped_html_script_tag() {
    let h = heap();
    blaze_sim::control(Config::cases(64), false, true, "", |tape, plant| {
        let src = if tape.coin("in_heading") {
            "# Hello <script>boom()</script>\n"
        } else {
            "Body with <script>boom()</script> text.\n"
        };
        let doc = parse_in(src, MarkdownLimits::SHIPPED, h.alloc()).expect("parse");
        let mut html = OwnString::new_in(h.alloc());
        doc.render_html_into_planted(&mut html, plant).expect("render");
        if html.as_str().contains("<script") {
            return Err(Broke::Oracle(
                format_in!(h.alloc(), "unescaped <script leaked: {:?}", html.as_str())
                    .as_str()
                    .into(),
            ));
        }
        Ok(())
    });
}
