//! WHAT A BODY'S VERDICT SAYS, as the lines `check` prints: the one home of a refusal's cause
//! and words (blaze-cli body_types.rs prints them for today's path, the body task carries them
//! in its result for `blaze engine-check`: tables/task.rs [`BodyOut::said`]).
//!
//! [`BodyOut::said`]: super::tables::task::BodyOut::said

use std::format;
use std::string::String;

use super::project::Refused;
use super::source::NamesSource;
use super::keys::Keys;

/// A refused body's words after the body's name (`check` prints `body `<name>`: <words>`): the
/// refusal and, for a head that did not resolve, its item by name and its text from the
/// records (rule 19: a key alone says nothing to a reader).
pub fn refused_words<'n, S: NamesSource<'n> + Copy>(names: S, refused: &Refused) -> String {
    let named = match refused {
        Refused::Check(super::check::CheckFail::Head { error: super::heads::HeadError::Item(key, _), .. }) => names.text_of(*key),
        _ => None,
    };
    match named {
        Some(it) => {
            let quoted = it.text.get(..it.text.len().min(240)).unwrap_or(it.text);
            format!("{}: {refused:?}; the item is `{}`: {quoted:?}", refused.text(), it.name.unwrap_or("_"))
        }
        None => format!("{}: {refused:?}", refused.text()),
    }
}

/// A refused body's cause: the line its refusal groups under. Rule 19: by WHAT was refused
/// (the construct and why, the head and its item by path, the check's class), never by
/// "lowering refused" or "check stopped" alone (port27/mini/clap-root, 2026-11-17).
pub fn refused_cause<'n, S: NamesSource<'n> + Copy>(names: S, keys: &Keys, refused: &Refused) -> String {
    use super::body::BodyError;
    match refused {
        // A head the body needs: which head (its item by path) and why; one line per head and
        // reason (no key, no range: those stay in the words).
        Refused::Lower(error @ BodyError::Head { item, error: head }) => {
            let at = match head {
                super::heads::HeadError::Item(k, _) => *k,
                _ => *item,
            };
            let shown = super::item_shown::ItemShown::of_lib(names, at);
            let path = shown.path().unwrap_or("an item the names source holds no path for").replace('\x1f', " ");
            let class = super::reach::lower_class(error).replace('\x1f', " ");
            format!("{} ({class}, in the head of `{path}`)", refused.text())
        }
        Refused::Lower(error) => format!("{} ({})", refused.text(), super::reach::lower_refusal(error).replace('\x1f', " ")),
        Refused::Check(fail) => {
            let class = super::item_shown::check_fail_class(names, keys, fail);
            format!("{} ({})", refused.text(), class.replace('\x1f', " "))
        }
        _ => format!("{}", refused.text()),
    }
}

/// The cause a body with gaps groups under.
pub const GAP_CAUSE: &str = "a question the front end cannot answer yet";

/// A body's gaps' words after its name: how many, and the first.
pub fn gap_words(gaps: &[super::world_items::Gap]) -> Option<String> {
    let first = gaps.first()?;
    Some(std::fmt::format(format_args!("{} questions the front end cannot answer yet, the first {first}", gaps.len())))
}
