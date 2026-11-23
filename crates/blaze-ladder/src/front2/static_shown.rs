//! A static named in a refusal (rule 19: an error names the thing). A module-level static is
//! its path (row_path.rs `shown_name`, as reach.rs `fn_shown`); a static declared in a BODY has no path in the names library (the
//! hello's `static MANIFEST`, which bstd's `main!` writes inside the `fn main` it makes), so it
//! is named by its own written head (`static MANIFEST: $crate::grant::Manifest`), the item whose
//! body declares it, and its key. Before this the refusal named it by its key alone
//! ("front2: `0x25421fdb..`, of the program's own crate ..", hello runner 8, 2026-11-16 18:40).

use std::format;
use std::string::String;

use blaze_typeck::ItemKey;

use super::check::Records;
use super::heads::Store;
use super::keys::Keys;
use super::source::NamesSource;

/// The most characters of a local static's head shown (its declaration up to `=`); the rest
/// of a longer head is said to be cut, never dropped silently.
const MOST_HEAD: usize = 200;

/// Static `k` as a refusal names it: a module-level static's path, or a local static's written
/// head with the item it is declared in and its key in hex.
pub(crate) fn static_shown(r: &Records<'_, '_>, k: ItemKey) -> String {
    static_shown_in(r.names, &*r.keys, &*r.store, k)
}

/// [`static_shown`] over the parts a lazy program holds (its names, key map and store): the one
/// home both the eager program.rs `made_statics` and program/lazy/fill.rs name a refused static
/// by. A static a macro writes in a block (tracing's `callsite2!` `static __CALLSITE`) has no
/// path; named by `ItemShown` alone it was "an item the names source holds no path for".
pub(crate) fn static_shown_in<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, k: ItemKey) -> String {
    // reach.rs `fn_shown` over these parts: the one printer of an item's name.
    let shown = |k: ItemKey| super::row_path::shown_name(names, keys, store, k);
    let Some(local) = store.locals.head(k) else { return shown(k) };
    let enclosing = shown(local.enclosing);
    match static_name(local.text) {
        Some(name) => format!("static `{name}`, declared in the body of `{enclosing}` (key {:#034x})", k.0),
        // No `static <name>` in its text (not reached for a kept static row): the text is shown,
        // cut and said so, never a guessed name.
        None => {
            let mut text: String = local.text.chars().take(MOST_HEAD).collect();
            if local.text.chars().count() > MOST_HEAD {
                text.push_str(&format!(" (text cut at {MOST_HEAD} characters)"));
            }
            format!("a static whose text names none (`{text}`), declared in the body of `{enclosing}` (key {:#034x})", k.0)
        }
    }
}

/// The name a static's text declares: the identifier after the keyword `static` (and after
/// `mut`), read outside string literals (a `#[doc = ".."]` may say "static") and `//` comments,
/// however the text is spaced: an expansion's text is printed token by token on one line
/// (`# [doc = ".."] static MANIFEST : $crate :: grant :: Manifest = ..`). A loop over bytes; the
/// slices end at ASCII bytes only, so they are on char boundaries.
pub(super) fn static_name(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    let mut after_static = false;
    while at < bytes.len() {
        let c = bytes[at];
        if c == b'"' {
            at += 1;
            while at < bytes.len() && bytes[at] != b'"' {
                at += if bytes[at] == b'\\' { 2 } else { 1 };
            }
            at += 1;
            continue;
        }
        if c == b'/' && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || bytes[at] >= 0x80) {
                at += 1;
            }
            let word = text.get(start..at)?;
            match (after_static, word) {
                (false, "static") => after_static = true,
                (true, "mut") => {}
                (true, name) => return Some(name),
                (false, _) => {}
            }
            continue;
        }
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    /// An expansion's one-line text, a doc attribute saying "static" first: the name is
    /// `MANIFEST` (the Reference, "Static items": `static` then the identifier).
    /// Expected: passes; before the fix the shown head was "# [ doc".
    #[test]
    fn static_name_past_attributes() {
        let t = "# [doc = \" a static manifest \"] static MANIFEST : $crate :: grant :: Manifest = $crate :: grant :: Manifest :: new (& []) ;";
        assert_eq!(super::static_name(t), Some("MANIFEST"));
        assert_eq!(super::static_name("/// static docs\nstatic mut N: u8 = 0;"), Some("N"));
        assert_eq!(super::static_name("const X: u8 = 0;"), None);
    }
}
