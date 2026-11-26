use blaze_own::testing::heap;

use crate::{
    ColorScheme, HtmlLimits, RenderBlock, TextAlign, decode_entities, parse_html, render_html,
};

#[test]
fn decodes_named_and_numeric_entities() {
    let h = heap();
    let a = h.alloc();
    assert_eq!(
        decode_entities("Install &middot; Docs &amp; &lt;code&gt; &#x2192; &#65;", a).as_str(),
        "Install · Docs & <code> → A"
    );
}

#[test]
fn renders_readme_hero_picture_dark_and_light() {
    let h = heap();
    let a = h.alloc();
    let html = r##"
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/fire/header-dark.webp">
    <img alt="blaze" src="docs/brand/fire/header-light.webp" width="344" height="184">
  </picture>
</p>
<h3 align="center">Rebuild Rust in milliseconds.</h3>
<p align="center">Type <code>blaze</code> wherever you'd type <code>cargo</code>.<br>Blazingly fast Rust, literally.</p>
<p align="center">
  <a href="#install">Install</a> ·
  <a href="#try-it">Try it</a> ·
  <a href="#learn-more">Docs</a>
</p>
"##;

    let dark = render_html(html, ColorScheme::Dark, HtmlLimits::SHIPPED, 64, a);
    assert!(matches!(
        dark.first(),
        Some(RenderBlock::Image { src, alt, width: Some(344), height: Some(184), align: TextAlign::Center })
            if src.as_str() == "docs/brand/fire/header-dark.webp" && alt.as_str() == "blaze"
    ));

    let light = render_html(html, ColorScheme::Light, HtmlLimits::SHIPPED, 64, a);
    assert!(matches!(
        light.first(),
        Some(RenderBlock::Image { src, .. }) if src.as_str() == "docs/brand/fire/header-light.webp"
    ));

    // Centered heading.
    assert!(matches!(
        dark.get(1),
        Some(RenderBlock::Heading { level: 3, align: TextAlign::Center, spans })
            if spans.first().is_some_and(|s| s.text.as_str() == "Rebuild Rust in milliseconds.")
    ));

    // `<br>` splits into two centered paragraphs, preserving `<code>` spans.
    let Some(RenderBlock::Paragraph { align: TextAlign::Center, spans: line1 }) = dark.get(2) else {
        panic!("expected first centered paragraph");
    };
    assert!(line1.iter().any(|s| s.style.code && s.text.as_str() == "blaze"));
    assert!(line1.iter().any(|s| s.style.code && s.text.as_str() == "cargo"));

    let Some(RenderBlock::Paragraph { align: TextAlign::Center, spans: line2 }) = dark.get(3) else {
        panic!("expected second centered paragraph after <br>");
    };
    assert_eq!(line2.first().map(|s| s.text.as_str()), Some("Blazingly fast Rust, literally."));

    // Centered link nav row preserves `href` and separators.
    let Some(RenderBlock::Paragraph { align: TextAlign::Center, spans: nav }) = dark.get(4) else {
        panic!("expected centered nav links paragraph");
    };
    assert!(nav.iter().any(|s| s.style.link && s.text.as_str() == "Install" && s.href.as_deref() == Some("#install")));
    assert!(nav.iter().any(|s| s.style.link && s.text.as_str() == "Docs" && s.href.as_deref() == Some("#learn-more")));
}

#[test]
fn parses_html_tables_lists_code_and_skips_scripts_styles() {
    let h = heap();
    let a = h.alloc();
    let html = r#"<!DOCTYPE html>
<html>
<head><title>Ignored</title><style>body { color: red; }</style><script>alert(1 < 2)</script></head>
<body>
  <!-- hidden comment -->
  <h1>Hello <em>World</em></h1>
  <ul><li>One</li><li>Two <kbd>Cmd+H</kbd></li></ul>
  <table>
    <thead><tr><th>Crate</th><th align="right">Time</th></tr></thead>
    <tbody><tr><td><code>blaze</code></td><td align="right"><strong>20 ms</strong></td></tr></tbody>
  </table>
  <pre><code class="language-rust">fn main() {}</code></pre>
</body>
</html>"#;

    let blocks = render_html(html, ColorScheme::Dark, HtmlLimits::SHIPPED, 64, a);
    assert!(matches!(blocks.first(), Some(RenderBlock::Heading { level: 1, .. })));
    assert!(blocks.iter().any(|b| matches!(b, RenderBlock::Bullet { .. })));
    assert!(blocks.iter().any(|b| matches!(b, RenderBlock::Table(t) if t.headers.len() == 2 && t.rows.len() == 1 && t.alignments.get(1) == Some(&TextAlign::Right))));
    assert!(blocks.iter().any(|b| matches!(b, RenderBlock::CodeFence { lang, code } if lang.as_str() == "rust" && code.as_str() == "fn main() {}")));
    // Ensure `<style>`, `<script>`, `<title>`, and `<!-- hidden comment -->` never leak into output.
    for b in blocks.iter() {
        if let RenderBlock::Paragraph { spans, .. } = b {
            for sp in spans.iter() {
                assert!(!sp.text.as_str().contains("alert"));
                assert!(!sp.text.as_str().contains("color: red"));
                assert!(!sp.text.as_str().contains("hidden comment"));
            }
        }
    }
}

#[test]
fn sim_fuzz_never_panics_and_respects_bounds() {
    use blaze_sim::{Config, check};

    check(Config::cases(120), |t| {
        let h = heap();
        let a = h.alloc();
        let mut s = blaze_own::AString::new_in(a);
        let tokens = [
            "<p align=\"center\">",
            "</p>",
            "<div>",
            "</div>",
            "<picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"a.webp\"><img src=\"b.png\" alt=\"x\"></picture>",
            "<code>",
            "</code>",
            "<a href=\"#x\">",
            "</a>",
            "<br>",
            "<hr>",
            "<!-- comment -->",
            "<table><tr><th>A</th><td>B</td></tr></table>",
            "<script>1 < 2</script>",
            "&amp;&lt;&#x2713;",
            "hello world ",
            "<",
            ">",
            "</unclosed>",
        ];
        let count = usize::try_from(t.int_in(1..=48, "count")).unwrap_or(1);
        for _ in 0..count {
            let idx = usize::try_from(t.below(tokens.len() as u64, "tok")).unwrap_or(0);
            if let Some(piece) = tokens.get(idx) {
                s.push_str(piece);
            }
        }
        let limits = HtmlLimits { max_nodes: 256, max_depth: 32, max_attrs_per_tag: 16 };
        let doc = parse_html(s.as_str(), limits, a);
        assert!(doc.nodes.len() <= limits.max_nodes);
        let blocks = render_html(s.as_str(), ColorScheme::Dark, limits, 32, a);
        assert!(blocks.len() <= 32);
        Ok::<(), blaze_sim::Broke>(())
    });
}
