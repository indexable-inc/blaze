//! A static named in a refusal (rule 19: an error names the thing). A module-level static is
//! its path ([`fn_shown`]); a static declared in a BODY has no path in the names library (the
//! hello's `static MANIFEST`, which bstd's `main!` writes inside the `fn main` it makes), so it
//! is named by its own written head (`static MANIFEST: $crate::grant::Manifest`), the item whose
//! body declares it, and its key. Before this the refusal named it by its key alone
//! ("front2: `0x25421fdb..`, of the program's own crate ..", hello runner 8, 2026-11-16 18:40).

use std::format;
use std::string::String;

use blaze_typeck::ItemKey;

use super::check::Records;
use super::reach::fn_shown;

/// The most characters of a local static's head shown (its declaration up to `=`); the rest
/// of a longer head is said to be cut, never dropped silently.
const MOST_HEAD: usize = 200;

/// Static `k` as a refusal names it: a module-level static's path, or a local static's written
/// head with the item it is declared in and its key in hex.
pub(crate) fn static_shown(r: &Records<'_, '_>, k: ItemKey) -> String {
    let Some(local) = r.store.locals.head(k) else { return fn_shown(r, k) };
    let mut head = String::new();
    // The declaration's head: the lines that are not a comment or an attribute, joined by one
    // space, up to the initializer's `=`.
    'lines: for line in local.text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") || line.starts_with("#![") {
            continue;
        }
        for word in line.split_whitespace() {
            if word == "=" || word.starts_with('=') {
                break 'lines;
            }
            if !head.is_empty() {
                head.push(' ');
            }
            match word.split_once('=') {
                Some((before, _)) => {
                    head.push_str(before);
                    break 'lines;
                }
                None => head.push_str(word),
            }
        }
    }
    if head.chars().count() > MOST_HEAD {
        let cut: String = head.chars().take(MOST_HEAD).collect();
        head = format!("{cut} (head cut at {MOST_HEAD} characters)");
    }
    format!("`{head}`, declared in the body of `{}` (key {:#034x})", fn_shown(r, local.enclosing), k.0)
}
