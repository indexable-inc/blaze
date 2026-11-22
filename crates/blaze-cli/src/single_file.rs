//! Single-file Rust & Blaze script runner (`blaze run script.rs [-- args...]`,
//! `blaze check script.rs`, `blaze test script.rs`).
//!
//! Supports:
//! - Optional `#!...` shebang (e.g. `#!/usr/bin/env -S blaze run`), replaced with a blank line
//!   so diagnostic line numbers match the original `.rs` file 1-to-1.
//! - Optional RFC 3424 frontmatter (`---` ... `---` TOML block), also blanked in `src/main.rs`
//!   for exact line-number preservation.
//! - Automatic edition detection when frontmatter does not declare `edition`:
//!   `blaze-2026.11` for `bstd` scripts (`fn main(cx: ...)` / `bstd::`), `2024` otherwise.
//! - Deterministic content-addressed workspace under `<home>/.blaze/v2/scripts/<stem>-<hash16>/`,
//!   written only on content change so warm runs hit Blaze's cache immediately.

use core::fmt::Write as _;

use blaze_ids::ContentId;
use blaze_own::{String as OwnString, Vec as OwnVec};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, Fs, FsKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::run_package::Located;
use crate::store::Home;

/// Hash domain for mapping a script's path identity to its materialized workspace folder.
const SCRIPT_PATH_DOMAIN: &str = "blaze-cli single-file script path 1";

/// Returns `true` when `raw` names a single Rust source file (`*.rs`).
pub(crate) fn is_script_arg(raw: &str) -> bool {
    raw.ends_with(".rs") && raw.len() > 3
}

/// Parsed single-file script: synthesized `Cargo.toml` text and line-preserving `src/main.rs` text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Synthesized<'f> {
    pub(crate) package_name: OwnString<&'f Frame>,
    pub(crate) manifest: OwnString<&'f Frame>,
    pub(crate) source: OwnString<&'f Frame>,
}

/// Extract the file stem of `raw` without `std::path::Path`.
fn file_stem(raw: &str) -> &str {
    let base = match raw.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => raw,
    };
    base.strip_suffix(".rs").unwrap_or(base)
}

/// Sanitize a file stem into a valid Cargo package name (`[a-z0-9_-]`, starting with ASCII letter).
fn sanitize_package_name<'f>(stem: &str, frame: &'f Frame) -> OwnString<&'f Frame> {
    let mut raw = OwnString::with_capacity_in(stem.len().max(6), frame);
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            raw.push(ch.to_ascii_lowercase());
        } else {
            raw.push('-');
        }
    }
    let trimmed = raw.as_str().trim_matches(|c| c == '-' || c == '_');
    let mut out = OwnString::with_capacity_in(trimmed.len() + 7, frame);
    if trimmed.is_empty() || !trimmed.as_bytes()[0].is_ascii_alphabetic() {
        out.push_str("script-");
        if trimmed.is_empty() {
            out.push_str("main");
        } else {
            out.push_str(trimmed);
        }
    } else {
        out.push_str(trimmed);
    }
    out
}

/// Detect whether `body` is a Blaze-edition (`bstd`) script or a standard Rust 2024 script.
fn detect_edition(body: &str) -> &'static str {
    let uses_bstd_main = body.contains("fn main(cx:")
        || body.contains("fn main(cx :")
        || body.contains("fn main(_cx:")
        || body.contains("fn main(_cx :")
        || body.contains("bstd::");
    if uses_bstd_main { "blaze-2026.11" } else { "2024" }
}

/// Parse a single-file `.rs` script into its `Cargo.toml` and line-preserving `src/main.rs`.
pub(crate) fn synthesize<'f>(stem: &str, raw_text: &str, frame: &'f Frame) -> Synthesized<'f> {
    let package_name = sanitize_package_name(stem, frame);

    let mut lines: OwnVec<&str, &'f Frame> = OwnVec::new_in(frame);
    for line in raw_text.split('\n') {
        lines.push(line.strip_suffix('\r').unwrap_or(line));
    }

    let mut out_lines: OwnVec<&str, &'f Frame> = OwnVec::with_capacity_in(lines.len(), frame);
    let mut idx = 0usize;

    // 1. Optional `#!...` shebang on line 0 (blanked to preserve line numbers).
    if let Some(first) = lines.first()
        && first.starts_with("#!")
        && !first.starts_with("#![")
    {
        out_lines.push("");
        idx = 1;
    }

    // Preserve blank lines before frontmatter.
    while idx < lines.len() && lines[idx].trim().is_empty() {
        out_lines.push(lines[idx]);
        idx += 1;
    }

    // 2. Optional RFC 3424 frontmatter: `---` (3+ dashes) ... matching closing dashes.
    let mut frontmatter = OwnString::new_in(frame);
    if idx < lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed.len() >= 3 && trimmed.bytes().all(|b| b == b'-') {
            let fence = trimmed;
            if let Some(rel_close) = lines[idx + 1..].iter().position(|l| l.trim() == fence) {
                let close = idx + 1 + rel_close;
                for line in &lines[idx + 1..close] {
                    frontmatter.push_str(line);
                    frontmatter.push('\n');
                }
                // Blank out the entire frontmatter block so diagnostics keep exact line numbers.
                for _ in idx..=close {
                    out_lines.push("");
                }
                idx = close + 1;
            }
        }
    }

    while idx < lines.len() {
        out_lines.push(lines[idx]);
        idx += 1;
    }

    let mut source = OwnString::with_capacity_in(raw_text.len(), frame);
    for (i, line) in out_lines.iter().enumerate() {
        if i > 0 {
            source.push('\n');
        }
        source.push_str(line);
    }

    let fm_trim = frontmatter.as_str().trim();
    let has_package = fm_trim.contains("[package]");
    let has_edition = fm_trim
        .lines()
        .any(|l| l.trim_start().starts_with("edition") && l.contains('='));

    let mut manifest = OwnString::with_capacity_in(fm_trim.len() + 128, frame);
    if has_package {
        manifest.push_str(fm_trim);
        manifest.push('\n');
    } else {
        let edition = detect_edition(source.as_str());
        manifest.push_str("[package]\n");
        let _ = writeln!(manifest, "name = \"{}\"", package_name.as_str());
        manifest.push_str("version = \"0.1.0\"\n");
        if !has_edition {
            let _ = writeln!(manifest, "edition = \"{edition}\"");
        }
        if !fm_trim.is_empty() {
            manifest.push('\n');
            manifest.push_str(fm_trim);
            manifest.push('\n');
        }
    }

    Synthesized { package_name, manifest, source }
}

/// Write `bytes` to `dir/path` only if the existing file is missing or differs in content.
fn write_if_changed(cx: &Cx, dir: &Dir<ReadWrite>, path: &str, bytes: &[u8], frame: &Frame) -> Result<(), CliError> {
    let p = Path::new(path)?;
    let mut existing = Bytes::new_in(frame, "single_file existing");
    match dir.read_all(cx, &p, &mut existing) {
        Ok(_) if existing.as_slice() == bytes => return Ok(()),
        Ok(_) => {}
        Err(err) if matches!(err.kind, FsKind::NotFound) => {}
        Err(err) => return Err(err.into()),
    }
    dir.publish(cx, &p, Content::bytes(bytes), Durable::No)?;
    Ok(())
}

/// Read an absolute script path through `Fs::home` over its parent directory (pure `bstd::fs`).
fn read_absolute(cx: &Cx, fs: &Fs, raw: &str, out: &mut Bytes<'_>) -> Result<(), CliError> {
    let Some((folder, file)) = raw.rsplit_once('/') else {
        return Err(CliError::Text { what: "single-file script path has no folder" });
    };
    let Some((parent, name)) = folder.rsplit_once('/') else {
        return Err(CliError::Text { what: "single-file script folder is not beneath a directory" });
    };
    let parent = if parent.is_empty() { "/" } else { parent };
    let dir = fs.home(cx, parent, &Path::new(name)?).map_err(|_| CliError::Text {
        what: "single-file script directory could not be opened",
    })?;
    dir.read_all(cx, &Path::new(file)?, out)?;
    Ok(())
}

/// Materialize a single `.rs` script into `<home>/.blaze/v2/scripts/<stem>-<hash16>/` and return
/// its `Located` workspace root so `blaze run`, `blaze check`, and `blaze test` execute it directly.
pub(crate) fn prepare<'f>(
    cx: &Cx,
    fs: &Fs,
    home_var: Option<&str>,
    raw: &str,
    frame: &'f Frame,
) -> Result<Located<'f>, CliError> {
    let mut raw_bytes = Bytes::new_in(frame, "single_file source");
    let key_str: &str = if raw.starts_with('/') {
        read_absolute(cx, fs, raw, &mut raw_bytes)?;
        raw
    } else {
        let mut clean = OwnString::with_capacity_in(raw.len(), frame);
        for part in raw.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if !clean.is_empty() {
                clean.push('/');
            }
            clean.push_str(part);
        }
        let rel = crate::work::held_str(clean.as_str(), frame);
        fs.root().read_all(cx, &Path::new(rel)?, &mut raw_bytes)?;
        rel
    };

    let raw_text = core::str::from_utf8(raw_bytes.as_slice()).map_err(|_| CliError::Text {
        what: "single-file script is not valid UTF-8",
    })?;

    let stem = file_stem(raw);
    let synth = synthesize(stem, raw_text, frame);

    // Deterministic workspace directory under `<home>/.blaze/v2/scripts/<pkg>-<hash16>`.
    let id = ContentId::of(SCRIPT_PATH_DOMAIN, key_str.as_bytes());
    let mut hex = OwnString::with_capacity_in(16, frame);
    for b in &id.0[..8] {
        let _ = write!(hex, "{b:02x}");
    }
    let rel_ws = bstd::mem::format_in!(frame, "v2/scripts/{}-{}", synth.package_name.as_str(), hex.as_str());
    let manifest_rel = bstd::mem::format_in!(frame, "{}/Cargo.toml", rel_ws.as_str());
    let main_rel = bstd::mem::format_in!(frame, "{}/src/main.rs", rel_ws.as_str());

    let home = Home::of(cx, fs, home_var)?;
    write_if_changed(cx, home.dir(), manifest_rel.as_str(), synth.manifest.as_bytes(), frame)?;
    write_if_changed(cx, home.dir(), main_rel.as_str(), synth.source.as_bytes(), frame)?;
    let ws_dir = home.dir().subdir(&Path::new(rel_ws.as_str())?);

    let abs_at = match home_var {
        Some(h) => bstd::mem::format_in!(frame, "{}/.blaze/{}", h.trim_end_matches('/'), rel_ws.as_str()),
        None => bstd::mem::format_in!(frame, ".blaze/{}", rel_ws.as_str()),
    };
    let at = crate::work::held_str(abs_at.as_str(), frame);
    Ok(Located { root: ws_dir, at, member: "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebang_and_frontmatter_preserve_line_numbers() {
        let frame = Frame::new(bstd::mem::Tag::Other).expect("frame");
        let script = "#!/usr/bin/env -S blaze run\n---\nedition = \"blaze-2026.11\"\n---\nfn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    out.line(cx, \"hi\")?;\n    Ok(())\n}\n";
        let s = synthesize("hello_script", script, &frame);
        assert_eq!(s.package_name.as_str(), "hello_script");
        assert!(s.manifest.as_str().contains("edition = \"blaze-2026.11\""));
        let orig_line = script.lines().position(|l| l.starts_with("fn main")).expect("fn main");
        let gen_line = s.source.as_str().lines().position(|l| l.starts_with("fn main")).expect("fn main");
        assert_eq!(orig_line, gen_line, "line numbers must match 1-to-1");
    }

    #[test]
    fn auto_detects_blaze_vs_rust_edition() {
        let frame = Frame::new(bstd::mem::Tag::Other).expect("frame");
        let bstd_script = "fn main(cx: &Cx, out: Out) -> Result<(), Error> { out.line(cx, \"ok\")?; Ok(()) }\n";
        assert!(synthesize("a", bstd_script, &frame).manifest.as_str().contains("edition = \"blaze-2026.11\""));

        let rust_script = "fn main() { println!(\"ok\"); }\n";
        assert!(synthesize("b", rust_script, &frame).manifest.as_str().contains("edition = \"2024\""));
    }
}
