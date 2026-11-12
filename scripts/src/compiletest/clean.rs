//! `tests/incremental`'s assertions on rustc's dependency graph, read for what they mean to
//! blaze. Pure.
//!
//! `#[rustc_clean(cfg = "rev")]` says rustc recomputed nothing for the item in that revision;
//! `except = "q1, q2"` names the rustc queries that were recomputed; `loaded_from_disk` names
//! queries read back from its cache. The queries are rustc's. What has a blaze meaning: on a
//! function, no `except` means its code is not made again in that revision, and an `except`
//! that names `optimized_mir` means it is. Every other form (an item that is no function, an
//! `except` of front-end queries only, `loaded_from_disk`, `rustc_partition_*`,
//! `rustc_if_this_changed`) names rustc's own graph or codegen units and is counted as not held.
//!
//! blaze refuses the attributes themselves (as rustc does without `-Z query-dep-graph`), so
//! the copy blaze builds has them written over with spaces, every line and column kept.

/// What one `#[rustc_clean]` asks of a revision's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hint {
    /// The function's code is not made again.
    Reused { revision: String, function: String },
    /// The function's code is made again.
    Remade { revision: String, function: String },
    /// No blaze meaning: why.
    NotHeld { why: &'static str },
}

/// The attributes that assert on rustc's dependency graph and codegen units.
const RUSTC_ASSERTS: &[&str] =
    &["rustc_clean", "rustc_partition_reused", "rustc_partition_codegened", "rustc_expected_cgu_reuse", "rustc_if_this_changed", "rustc_then_this_would_need"];

/// Where the attribute that starts at `start` (its `#`) ends: after the `]` that closes it.
fn attribute_end(source: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in source.as_bytes()[start..].iter().enumerate() {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Each of [`RUSTC_ASSERTS`] in `source`: (its name, where it starts, where it ends).
fn asserts(source: &str) -> Vec<(&'static str, usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = source[at..].find('#') {
        let start = at + found;
        at = start + 1;
        let Some(inner) = source[start + 1..].trim_start_matches('!').strip_prefix('[') else { continue };
        let Some(name) = RUSTC_ASSERTS.iter().find(|name| inner.strip_prefix(**name).is_some_and(|it| it.starts_with(['(', ']']))) else { continue };
        let Some(end) = attribute_end(source, start) else { continue };
        out.push((*name, start, end));
        at = end;
    }
    out
}

/// `source` with each of [`RUSTC_ASSERTS`] written over with spaces: every other byte, and
/// every line and column, as it was.
pub(crate) fn blanked(source: &str) -> String {
    let mut out = source.as_bytes().to_vec();
    for (_, start, end) in asserts(source) {
        for byte in &mut out[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // Whole characters were written over: an attribute's bytes from its `#` to its `]`.
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of `key = ".."` in an attribute's text.
fn value<'a>(attribute: &'a str, key: &str) -> Option<&'a str> {
    let after = &attribute[attribute.find(key)? + key.len()..];
    let after = after.trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
    after.split('"').next()
}

/// The name of the function the item after `rest` declares, if it is one: attributes, comments
/// and blank lines are passed over, then `fn NAME` after its qualifiers.
fn function_after(rest: &str) -> Option<String> {
    let mut rest = rest.trim_start();
    loop {
        if rest.starts_with("#[") || rest.starts_with("#![") {
            rest = rest[attribute_end(rest, 0)?..].trim_start();
        } else if rest.starts_with("//") {
            rest = rest.split_once('\n')?.1.trim_start();
        } else {
            break;
        }
    }
    let mut words = rest.split(|c: char| c.is_whitespace() || c == '(' || c == '<');
    // bound: the qualifiers before `fn` (`pub(crate)`, `const`, `async`, `unsafe`, `extern "C"`).
    for _ in 0..8 {
        match words.next()? {
            "fn" => return words.find(|it| !it.is_empty()).map(str::to_owned),
            word if word.starts_with("pub") || word.is_empty() || ["const", "async", "unsafe", "extern", "default", "crate)", "super)"].contains(&word) || word.starts_with('"') => {}
            _ => return None,
        }
    }
    None
}

/// What `source`'s `rustc_*` assertions ask, one hint each.
pub(crate) fn hints(source: &str) -> Vec<Hint> {
    asserts(source)
        .into_iter()
        .map(|(name, start, end)| {
            if name != "rustc_clean" {
                return Hint::NotHeld { why: "rustc's codegen units or dependency edges" };
            }
            let attribute = &source[start..end];
            let Some(revision) = value(attribute, "cfg") else {
                return Hint::NotHeld { why: "a rustc_clean with no cfg" };
            };
            if attribute.contains("loaded_from_disk") {
                return Hint::NotHeld { why: "loaded_from_disk: rustc's on-disk query cache" };
            }
            let Some(function) = function_after(&source[end..]) else {
                return Hint::NotHeld { why: "an item that is no function: no code of its own" };
            };
            match value(attribute, "except") {
                None => Hint::Reused { revision: revision.to_owned(), function },
                Some(except) if except.split(',').any(|it| it.trim() == "optimized_mir") => Hint::Remade { revision: revision.to_owned(), function },
                Some(_) => Hint::NotHeld { why: "an except of rustc's front-end queries only" },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "#![rustc_partition_reused(module = \"a\", cfg = \"rpass2\")]\n\
#[cfg(not(rpass1))]\n\
#[rustc_clean(cfg = \"rpass2\")]\n\
// a comment\n\
#[inline]\n\
pub(crate) unsafe fn kept<T>(x: T) {}\n\
#[rustc_clean(cfg=\"rpass2\", except=\"opt_hir_owner_nodes, optimized_mir\")]\n\
fn changed() {}\n\
#[rustc_clean(cfg = \"rpass3\", except = \"typeck\")]\n\
fn typed() {}\n\
#[rustc_clean(cfg = \"rpass2\")]\n\
struct S { x: [u8; 2] }\n\
#[rustc_clean(cfg = \"rpass2\", loaded_from_disk = \"typeck\")]\n\
fn loaded() {}\n\
#[derive(Clone)] struct T;\n";

    #[test]
    fn a_rustc_clean_is_read_for_what_it_means_to_blaze() {
        let said = hints(SOURCE);
        assert_eq!(said.len(), 6, "{said:?}");
        assert!(matches!(said[0], Hint::NotHeld { .. }));
        assert_eq!(said[1], Hint::Reused { revision: "rpass2".to_owned(), function: "kept".to_owned() });
        assert_eq!(said[2], Hint::Remade { revision: "rpass2".to_owned(), function: "changed".to_owned() });
        assert!(matches!(said[3], Hint::NotHeld { why } if why.contains("front-end")));
        assert!(matches!(said[4], Hint::NotHeld { why } if why.contains("no function")));
        assert!(matches!(said[5], Hint::NotHeld { why } if why.contains("loaded_from_disk")));
    }

    #[test]
    fn blanking_keeps_every_line_and_column() {
        let out = blanked(SOURCE);
        assert_eq!(out.len(), SOURCE.len());
        assert!(!out.contains("rustc_"), "{out}");
        assert_eq!(out.lines().count(), SOURCE.lines().count());
        for (before, after) in SOURCE.lines().zip(out.lines()) {
            assert_eq!(before.len(), after.len());
            if !before.contains("rustc_") {
                assert_eq!(before, after);
            }
        }
        assert!(out.contains("#[cfg(not(rpass1))]") && out.contains("#[derive(Clone)] struct T;"));
    }
}
