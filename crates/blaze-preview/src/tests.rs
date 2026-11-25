extern crate std;

use crate::{MdBlock, MdStyle, PreviewBody, PreviewLimits, TokenKind, build_preview};

#[test]
fn previews_uv_lock_as_clean_package_table() {
    let raw = r#"
version = 1
revision = 5

[[package]]
name = "blake3"
version = "1.0.11"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "blaze-scripts"
version = "0.1.0"
source = { virtual = "." }
"#;
    let alloc = blaze_own::testing::heap().alloc();
    let doc = build_preview("uv.lock", raw.as_bytes(), raw.len() as u64, PreviewLimits::SHIPPED, alloc);
    let PreviewBody::Table(tbl) = doc.body else {
        panic!("expected tabular preview for uv.lock, got {:?}", doc.body);
    };
    assert_eq!(tbl.headers.len(), 3);
    assert_eq!(tbl.rows.len(), 2);
    assert_eq!(tbl.rows[0][0].as_str(), "blake3");
    assert_eq!(tbl.rows[0][1].as_str(), "1.0.11");
    assert_eq!(tbl.rows[0][2].as_str(), "pypi");
    assert_eq!(tbl.rows[1][0].as_str(), "blaze-scripts");
    assert_eq!(tbl.rows[1][2].as_str(), "workspace");
}

#[test]
fn previews_markdown_as_rendered_blocks_with_inline_formatting() {
    let md = "# blaze\n\nFast **incremental** compiler with `zero` lag.\n\n- item one\n- item two\n\n```rust\nfn main() { let s = \"hi\"; }\n```\n";
    let alloc = blaze_own::testing::heap().alloc();
    let doc = build_preview("README.md", md.as_bytes(), md.len() as u64, PreviewLimits::SHIPPED, alloc);
    let PreviewBody::Markdown { blocks, .. } = doc.body else {
        panic!("expected rendered markdown preview");
    };
    assert!(matches!(blocks[0], MdBlock::Heading { level: 1, .. }));
    assert!(blocks.iter().any(|b| matches!(b, MdBlock::Bullet { .. })));
    assert!(blocks.iter().any(|b| matches!(b, MdBlock::CodeFence { .. })));
    let para = blocks.iter().find_map(|b| match b {
        MdBlock::Paragraph { spans } => Some(spans),
        _ => None,
    }).expect("paragraph");
    assert!(para.iter().any(|s| s.style == MdStyle::Bold && s.text.as_str() == "incremental"));
    assert!(para.iter().any(|s| s.style == MdStyle::Code && s.text.as_str() == "zero"));
}

#[test]
fn previews_rust_code_with_syntax_tokens() {
    let rs = "pub fn hello() -> &'static str {\n    // greet\n    \"world\"\n}\n";
    let alloc = blaze_own::testing::heap().alloc();
    let doc = build_preview("main.rs", rs.as_bytes(), rs.len() as u64, PreviewLimits::SHIPPED, alloc);
    let PreviewBody::Code { lang, lines, .. } = doc.body else {
        panic!("expected syntax-highlighted code preview");
    };
    assert_eq!(lang, "Rust");
    assert!(lines[0].spans.iter().any(|s| s.kind == TokenKind::Keyword && s.text.as_str() == "fn"));
    assert!(lines[1].spans.iter().any(|s| s.kind == TokenKind::Comment));
    assert!(lines[2].spans.iter().any(|s| s.kind == TokenKind::String && s.text.as_str() == "\"world\""));
}

#[test]
fn sim_preview_never_panics_on_arbitrary_inputs() {
    use blaze_sim::{Config, check};

    check(Config::cases(120), |tape| {
        let alloc = blaze_own::testing::heap().alloc();
        let n = tape.int_in(0..=96, "len") as usize;
        let mut buf = std::vec::Vec::with_capacity(n);
        for _ in 0..n {
            buf.push(tape.int_in(0..=255, "b") as u8);
        }
        let names = ["uv.lock", "Cargo.toml", "README.md", "main.rs", "data.csv", "a.json", "bin"];
        let name = names[tape.below(names.len() as u64, "name") as usize];
        let _ = build_preview(name, &buf, n as u64, PreviewLimits::SHIPPED, alloc);
        Ok::<(), std::string::String>(())
    });
}
