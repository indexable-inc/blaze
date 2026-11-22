//! Structured compiler graph emitter for Blaze Explore, backed directly by `blaze-file-items`
//! and `blaze-surface` (`outline` + `impl_header`) with zero duplicated parsing logic.
//!
//! Usage:
//!   explore_graph <path.rs>
//!
//! Prints one JSON object to stdout:
//!   `{"items":[...],"uses":[...],"submods":[...]}`

use std::env;
use std::fs;
use std::io::{self, Write};

use blaze_file_items::{impl_header, outline};
use blaze_own::Alloc;
use blaze_surface::{AttrKind, Edition, ItemDetail, WrittenVis};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: explore_graph <file.rs>");
        std::process::exit(2);
    };
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
    };

    let alloc = Alloc::global();
    let out = outline(&text, Edition::E2024, alloc);
    let items = out.items();

    // Precompute line-start byte offsets for O(log N) byte-offset -> 1-based line lookup.
    let mut line_starts: Vec<u32> = Vec::with_capacity(text.len() / 32 + 16);
    line_starts.push(0);
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push((idx + 1) as u32);
        }
    }
    let byte_to_line = |off: u32| -> u32 {
        match line_starts.binary_search(&off) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i.max(1) as u32,
        }
    };

    // Resolve each Impl item's self_ty once via `blaze_file_items::impl_header`.
    let mut impl_owner: Vec<Option<String>> = vec![None; items.len()];
    for (i, item) in items.iter().enumerate() {
        if matches!(item.detail, ItemDetail::Impl) {
            let sig_str = strip_outer_attrs_prefix(&text, item, out.attrs(item));
            if let Ok(hdr) = impl_header(sig_str, Edition::E2024, alloc) {
                let base = hdr.self_ty.split('<').next().unwrap_or(hdr.self_ty).trim();
                if !base.is_empty() {
                    impl_owner[i] = Some(base.to_string());
                }
            }
        }
    }

    let mut json = String::with_capacity(items.len() * 160 + 256);
    json.push_str(r#"{"items":["#);
    let mut first_item = true;
    let mut uses: Vec<String> = Vec::new();
    let mut submods: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        match item.detail {
            ItemDetail::Use { tree: Some(tr) } => {
                let t = item.text(&text, tr).trim();
                if !t.is_empty() {
                    uses.push(t.to_string());
                }
                continue;
            }
            ItemDetail::Mod { inline: false, .. } => {
                if let Some(nr) = item.name {
                    let n = item.text(&text, nr).trim();
                    if !n.is_empty() {
                        submods.push(n.to_string());
                    }
                }
                continue;
            }
            ItemDetail::Error | ItemDetail::Unhandled { .. } | ItemDetail::MacroCall { .. } => {
                continue;
            }
            _ => {}
        }

        let parent_idx = item.parent.map(|p| p.at());
        let parent_item = parent_idx.and_then(|pi| items.get(pi));
        let owner: Option<String> = match (item.detail, parent_idx, parent_item) {
            (ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::TypeAlias, Some(pi), Some(p))
                if matches!(p.detail, ItemDetail::Impl) =>
            {
                impl_owner[pi].clone()
            }
            (ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::TypeAlias, _, Some(p))
                if matches!(p.detail, ItemDetail::Trait) =>
            {
                p.name.map(|nr| item.text(&text, nr).to_string())
            }
            (ItemDetail::Field { .. } | ItemDetail::Variant { .. }, _, Some(p)) => {
                p.name.map(|nr| p.text(&text, nr).to_string())
            }
            _ => None,
        };

        let kind = match item.detail {
            ItemDetail::Fn { .. } => {
                if owner.is_some() {
                    "method"
                } else {
                    "fn"
                }
            }
            ItemDetail::Struct => "struct",
            ItemDetail::Enum => "enum",
            ItemDetail::Union => "struct",
            ItemDetail::Trait | ItemDetail::TraitAlias => "trait",
            ItemDetail::Impl => "impl",
            ItemDetail::Const { .. } => "const",
            ItemDetail::Static { .. } => "static",
            ItemDetail::TypeAlias => "type",
            ItemDetail::Field { .. } => "field",
            ItemDetail::Variant { .. } => "variant",
            ItemDetail::MacroRules { .. } | ItemDetail::MacroDef => "macro",
            _ => continue,
        };

        let name = if matches!(item.detail, ItemDetail::Impl) {
            impl_owner[i].clone().unwrap_or_default()
        } else {
            item.name
                .map(|nr| item.text(&text, nr).trim().to_string())
                .unwrap_or_default()
        };
        if name.is_empty() {
            continue;
        }

        let item_attrs = out.attrs(item);
        // Exact declaration line (skipping leading doc comments / attributes!)
        let decl_off = item
            .name
            .map(|nr| item.in_file(nr).start)
            .unwrap_or_else(|| strip_outer_attrs_offset(&text, item, item_attrs));
        let line = byte_to_line(decl_off);
        let start_line = byte_to_line(item.range.start);
        let end_line = byte_to_line(item.range.end.saturating_sub(1).max(item.range.start));

        let sig_raw = match item.detail {
            ItemDetail::Field { ty: Some(tr), .. } => {
                format!("{name}: {}", collapse_ws(item.text(&text, tr)))
            }
            ItemDetail::Variant { .. } => {
                if let Some(ref ow) = owner {
                    format!("{ow}::{name}")
                } else {
                    name.clone()
                }
            }
            _ => collapse_ws(strip_outer_attrs_prefix(&text, item, item_attrs)),
        };

        let ty_str = match item.detail {
            ItemDetail::Field { ty: Some(tr), .. } => collapse_ws(item.text(&text, tr)),
            _ => String::new(),
        };

        // Collect doc comment lines from `blaze_surface::AttrKind::Doc`!
        let mut docs: Vec<String> = Vec::new();
        for attr in item_attrs {
            if matches!(attr.kind, AttrKind::Doc) && docs.len() < 2 {
                let raw_doc = item.text(&text, attr.range).trim();
                let cleaned = raw_doc
                    .strip_prefix("///")
                    .or_else(|| raw_doc.strip_prefix("//!"))
                    .unwrap_or(raw_doc)
                    .trim();
                if !cleaned.is_empty() {
                    docs.push(cleaned.to_string());
                }
            }
        }
        let doc = docs.join(" ");
        let is_pub = !matches!(item.visibility, WrittenVis::Private);

        if !first_item {
            json.push(',');
        }
        first_item = false;
        json.push_str(r#"{"kind":""#);
        json.push_str(kind);
        json.push_str(r#"","name":""#);
        push_esc(&mut json, &name);
        json.push_str(r#"","owner":""#);
        if let Some(ref ow) = owner {
            push_esc(&mut json, ow);
        }
        json.push_str(r#"","sig":""#);
        push_esc(&mut json, &sig_raw);
        json.push_str(r#"","ty":""#);
        push_esc(&mut json, &ty_str);
        json.push_str(r#"","doc":""#);
        push_esc(&mut json, &doc);
        json.push_str(r#"","line":"#);
        json.push_str(&line.to_string());
        json.push_str(r#","start_line":"#);
        json.push_str(&start_line.to_string());
        json.push_str(r#","end_line":"#);
        json.push_str(&end_line.to_string());
        json.push_str(r#","pub":"#);
        json.push_str(if is_pub { "true" } else { "false" });
        json.push('}');
    }

    json.push_str(r#"],"uses":["#);
    for (idx, u) in uses.iter().enumerate() {
        if idx > 0 {
            json.push(',');
        }
        json.push('"');
        push_esc(&mut json, u);
        json.push('"');
    }
    json.push_str(r#"],"submods":["#);
    for (idx, m) in submods.iter().enumerate() {
        if idx > 0 {
            json.push(',');
        }
        json.push('"');
        push_esc(&mut json, m);
        json.push('"');
    }
    json.push_str("]}");

    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(json.as_bytes());
}

fn strip_outer_attrs_offset(
    _text: &str,
    item: &blaze_surface::Item,
    item_attrs: &[blaze_surface::Attr],
) -> u32 {
    let mut rel_start = item.signature.start;
    for a in item_attrs {
        if !a.inner && a.range.end > rel_start {
            rel_start = a.range.end;
        }
    }
    item.range.start.saturating_add(rel_start)
}

fn strip_outer_attrs_prefix<'t>(
    text: &'t str,
    item: &blaze_surface::Item,
    item_attrs: &[blaze_surface::Attr],
) -> &'t str {
    let sig_file = item.in_file(item.signature);
    let mut start = sig_file.start;
    for a in item_attrs {
        if !a.inner {
            let af = item.in_file(a.range);
            if af.end > start && af.end <= sig_file.end {
                start = af.end;
            }
        }
    }
    text.get(start as usize..sig_file.end as usize)
        .unwrap_or("")
        .trim()
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn push_esc(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}
