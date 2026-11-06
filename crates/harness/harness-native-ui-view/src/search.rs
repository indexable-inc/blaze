//! A searchable menu (search_menu.flecs `ui.SearchMenu`): what is typed into its filter, which of its rows match it, and
//! its one highlight. Sans IO: the owner keeps a [`Search`], hands it typing and the arrow keys while the menu is open,
//! builds its rows as [`Entry`]s, lays out what [`filter`] keeps under the plate's `items`, and shows the query with
//! [`show`].
//!
//! Matching is fuzzy, an fzf-like subsequence: every character of the query, in order, case-insensitively, anywhere in
//! the row's words. A match on a word's start and a run of consecutive characters score higher, so `son` puts Sonnet
//! before a row that only has an s, an o and an n somewhere. Within a section the matches keep their order unless the
//! query is typed, then they go best first (ties in their order).
use crate::View;
use crate::menu::Nav;

/// The query and the highlight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    pub nav: Nav,
}

/// A change to the query.
#[derive(Clone, Copy, Debug)]
pub enum Edit<'a> {
    /// Text typed (control characters dropped).
    Type(&'a str),
    /// Backspace: the last character.
    Erase,
    /// Option-Backspace: the last word.
    EraseWord,
    /// Command-Backspace: all of it.
    Clear,
}

impl Search {
    /// Apply `e`. Whether the query changed (then the highlight goes to the first match: [`Search::first`]).
    pub fn edit(&mut self, e: Edit<'_>) -> bool {
        let before = self.query.len();
        match e {
            Edit::Type(s) => self.query.extend(s.chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))),
            Edit::Erase => {
                self.query.pop();
            }
            Edit::EraseWord => {
                let t = self.query.trim_end().len();
                let cut = self.query[..t].rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.query.truncate(cut);
            }
            Edit::Clear => self.query.clear(),
        }
        let changed = self.query.len() != before;
        if changed {
            self.nav = Nav::default();
        }
        changed
    }

    /// Highlight the first of `n` rows `choosable` allows when there is a query and nothing is highlighted, so Return
    /// picks the best match (an empty query leaves the menu as it opened).
    pub fn first(&mut self, n: usize, choosable: impl Fn(usize) -> bool) {
        if !self.query.is_empty() && self.nav.hot.is_none() {
            self.nav.step(true, n, choosable);
        }
    }
}

/// How well `query` matches `text` (higher is better), or None when it does not: every character of the query, in
/// order, case-insensitively. An empty query matches everything at 0.
#[must_use]
pub fn score(query: &str, text: &str) -> Option<u32> {
    let mut score = 0u32;
    let mut prev: Option<usize> = None;
    let chars: Vec<char> = text.chars().collect(); // alloc: one row's words, while a query is typed
    let mut at = 0;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let q = q.to_lowercase().next().unwrap_or(q);
        let i = (at..chars.len()).find(|&i| chars[i].to_lowercase().next() == Some(q))?;
        let word_start = i == 0 || !chars[i - 1].is_alphanumeric();
        score += 1 + if word_start { 8 } else { 0 } + if prev == Some(i.wrapping_sub(1)) { 4 } else { 0 };
        prev = Some(i);
        at = i + 1;
    }
    Some(score)
}

/// A row of a searchable menu.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry<T> {
    /// A section's head (kept while its section has a match).
    Head(String),
    /// A separator between sections (kept between sections that are kept).
    Sep,
    /// A choice, matched by `text` (its label and anything else it should be found by).
    Item { text: String, data: T },
    /// A line saying why nothing can be chosen (dropped once a query is typed).
    Note(String),
}

/// The rows of `rows` that `query` keeps: each section's matching items (best first), its head when any match, a
/// separator only between kept sections; notes go. With nothing matching, one note saying so. An empty query keeps all.
#[must_use]
pub fn filter<T>(query: &str, rows: Vec<Entry<T>>) -> Vec<Entry<T>> {
    if query.trim().is_empty() {
        return rows;
    }
    let mut out: Vec<Entry<T>> = Vec::with_capacity(rows.len()); // alloc: the open menu's rows, once a frame
    let mut head: Option<String> = None;
    let mut found: Vec<(u32, usize, Entry<T>)> = Vec::new(); // alloc: one section's matches
    let flush = |out: &mut Vec<Entry<T>>, head: &mut Option<String>, found: &mut Vec<(u32, usize, Entry<T>)>| {
        if found.is_empty() {
            *head = None;
            return;
        }
        if !out.is_empty() {
            out.push(Entry::Sep);
        }
        if let Some(h) = head.take() {
            out.push(Entry::Head(h));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        out.extend(found.drain(..).map(|(_, _, e)| e));
    };
    for (i, r) in rows.into_iter().enumerate() {
        match r {
            Entry::Head(h) => {
                flush(&mut out, &mut head, &mut found);
                head = Some(h);
            }
            Entry::Sep => flush(&mut out, &mut head, &mut found),
            Entry::Item { text, data } => {
                if let Some(s) = score(query, &text) {
                    found.push((s, i, Entry::Item { text, data }));
                }
            }
            Entry::Note(_) => {}
        }
    }
    flush(&mut out, &mut head, &mut found);
    if out.is_empty() {
        out.push(Entry::Note(format!("No match for \u{201c}{}\u{201d}", query.trim())));
    }
    out
}

/// Show `query` in `menu`'s filter (a `ui.SearchMenu`, or a plate with a `ui.MenuFilter` named `filter`), `hint` muted
/// after the caret while it is empty. Written only on change.
pub fn show(menu: View<'_>, query: &str, hint: &str) {
    let Some(f) = menu.part("filter") else { return };
    for (part, s) in [("text", query), ("hint", if query.is_empty() { hint } else { "" })] {
        if let Some(p) = f.part(part) {
            p.text(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(s: &str) -> Entry<()> {
        Entry::Item { text: s.to_owned(), data: () }
    }

    #[test]
    fn fuzzy_matches_in_order_and_prefers_word_starts() {
        assert!(score("son", "Sonnet").is_some());
        assert!(score("snt", "Sonnet").is_some());
        assert!(score("tns", "Sonnet").is_none(), "out of order");
        assert!(score("SON", "sonnet").is_some(), "case-insensitive");
        assert!(score("son", "Sonnet 5.5").unwrap() > score("son", "Opus · reasoning on").unwrap_or(0));
        assert!(score("g61", "GPT-6.1-Sol").is_some());
    }

    #[test]
    fn filtering_keeps_heads_and_separators_only_around_matches() {
        let rows = vec![
            Entry::Head("Claude".into()),
            item("Opus"),
            item("Sonnet"),
            Entry::Sep,
            Entry::Head("Codex".into()),
            item("GPT-6.1-Sol"),
            Entry::Note("why".into()),
        ];
        let kept = filter("son", rows.clone());
        assert_eq!(kept, vec![Entry::Head("Claude".into()), item("Sonnet")]);
        let kept = filter("o", rows.clone());
        assert_eq!(kept.iter().filter(|e| matches!(e, Entry::Sep)).count(), 1);
        assert_eq!(filter("", rows.clone()), rows);
        assert!(matches!(filter("zzz", rows).as_slice(), [Entry::Note(_)]));
    }

    #[test]
    fn editing_resets_the_highlight_and_first_picks_the_top_match() {
        let mut s = Search::default();
        s.nav.hot = Some(3);
        assert!(s.edit(Edit::Type("so\u{1b}")));
        assert_eq!((s.query.as_str(), s.nav.hot), ("so", None));
        s.first(4, |i| i != 0);
        assert_eq!(s.nav.hot, Some(1));
        s.edit(Edit::Type("n gpt"));
        assert!(s.edit(Edit::EraseWord));
        assert_eq!(s.query, "son ");
        assert!(s.edit(Edit::Clear));
        assert!(!s.edit(Edit::Erase));
    }
}
