//! Syntax highlighting for words marked `Highlight: {file}`: tree-sitter parses them in the language the file's name says
//! and its highlight query names each stretch (`function`, `string`, `operator`...). Which names are coloured, and in what
//! ink, is script: every entity under `code_ink` with a `Fill`, named by its path below it (`code_ink.punctuation.delimiter`
//! colours the capture `punctuation.delimiter`). A stretch no token names keeps the node's own ink.
use std::collections::HashMap;
use std::rc::Rc;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter as Ts};

/// The languages a `Highlight`'s file can be, by its extension or name.
fn language(file: &str) -> Option<(&'static str, tree_sitter::Language, &'static str)> {
    let name = file.rsplit('/').next().unwrap_or(file);
    let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
    match ext {
        "sh" | "bash" | "zsh" | "shell" => {
            Some(("bash", tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY))
        }
        "rs" | "rust" => {
            Some(("rust", tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY))
        }
        "ts" | "typescript" | "mts" | "cts" => {
            Some(("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), TS_QUERY.as_str()))
        }
        "tsx" => Some(("tsx", tree_sitter_typescript::LANGUAGE_TSX.into(), TS_QUERY.as_str())),
        "js" | "javascript" | "mjs" | "cjs" | "jsx" => Some((
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        )),
        "py" | "python" => {
            Some(("python", tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY))
        }
        "json" | "jsonc" => {
            Some(("json", tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY))
        }
        _ => None,
    }
}

/// TypeScript's highlight query extends JavaScript's: both, JavaScript's first.
static TS_QUERY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!("{}\n{}", tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY)
});

/// Whether blaze has a grammar for `file`'s kind (a code card colours only those).
#[must_use]
pub fn knows(file: &str) -> bool {
    language(file).is_some()
}

/// A stretch of the words, as UTF-16 units `start..end`, and which of the names it was asked about it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub name: usize,
}

#[derive(Default)]
pub struct Highlighter {
    ts: Option<Ts>,
    configs: HashMap<(&'static str, Vec<String>), Rc<HighlightConfiguration>>,
    spans: HashMap<(String, String, Vec<String>), Rc<[Span]>>,
}

impl Highlighter {
    /// `text`'s stretches that `names` (capture names, dotted) cover, as tree-sitter reads it in `file`'s language. Err
    /// names the file when blaze has no grammar for it.
    pub fn spans(
        &mut self,
        file: &str,
        text: &str,
        names: &[String],
    ) -> Result<Rc<[Span]>, String> {
        let key = (file.to_owned(), text.to_owned(), names.to_vec());
        if let Some(s) = self.spans.get(&key) {
            return Ok(s.clone());
        }
        let Some((lang_name, lang, query)) = language(file) else {
            return Err(format!(
                "Highlight {file:?}: no grammar for this file's kind (shell, rust, typescript, javascript, python, json)"
            ));
        };
        let config = match self.configs.get(&(lang_name, names.to_vec())) {
            Some(c) => c.clone(),
            None => {
                let mut c =
                    HighlightConfiguration::new(lang, lang_name, query, "", "").map_err(|e| {
                        format!("Highlight {file:?}: the {lang_name} highlight query: {e}")
                    })?;
                c.configure(names);
                let c = Rc::new(c);
                self.configs.insert((lang_name, names.to_vec()), c.clone());
                c
            }
        };
        // UTF-16 index of each byte offset, for the layout's glyph indices.
        let mut u16_at = Vec::with_capacity(text.len() + 1);
        let mut u = 0usize;
        for ch in text.chars() {
            for _ in 0..ch.len_utf8() {
                u16_at.push(u);
            }
            u += ch.len_utf16();
        }
        u16_at.push(u);
        let ts = self.ts.get_or_insert_with(Ts::new);
        let events = ts
            .highlight(&config, text.as_bytes(), None, |_| None)
            .map_err(|e| format!("Highlight {file:?}: {e}"))?;
        let (mut out, mut stack) = (Vec::new(), Vec::new());
        for ev in events {
            match ev.map_err(|e| format!("Highlight {file:?}: {e}"))? {
                HighlightEvent::HighlightStart(h) => stack.push(h.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    if let Some(&name) = stack.last() {
                        out.push(Span { start: u16_at[start], end: u16_at[end], name });
                    }
                }
            }
        }
        let spans: Rc<[Span]> = out.into();
        if self.spans.len() > 1024 {
            self.spans.clear();
        }
        self.spans.insert(key, spans.clone());
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chat's ```rust block (its file `code.rust`) colours its keyword, function name and string by the names asked about;
    /// asked about none, nothing is coloured (a scene with no `code_ink` draws code in one ink).
    #[test]
    fn a_fenced_rust_block_gets_its_captures() {
        let mut h = Highlighter::default();
        let names: Vec<String> = ["function", "keyword", "string"].map(str::to_owned).to_vec();
        let text = "fn reverse(s: &str) -> String { \"x\".into() }";
        let spans = h.spans("code.rust", text, &names).unwrap();
        let named = |n: usize| spans.iter().filter(|s| s.name == n).count();
        assert!(named(0) >= 1 && named(1) >= 1 && named(2) >= 1, "{spans:?}");
        assert!(h.spans("code.rust", text, &[]).unwrap().is_empty());
    }
}
