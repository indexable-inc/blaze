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
        _ => None,
    }
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
                "Highlight {file:?}: no grammar for this file's kind (only shell so far: .sh, .bash, .zsh)"
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
