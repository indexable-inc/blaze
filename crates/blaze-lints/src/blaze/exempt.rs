//! The comments that keep a cost on a line: `// clone: <why>`, `// alloc: <why>` and
//! `// dyn: <why>`, on a line of the finding or on the line above it, with a reason that isn't
//! empty. Read from the text of the tree walked, nothing allocated.

use syntax::TextRange;

use super::Lint;

/// The word of the comment that keeps `lint`'s cost; none for the parallel lints, which say
/// what a loop could be rather than what a line costs.
const fn tag(lint: Lint) -> Option<&'static str> {
    match lint {
        Lint::HotClone => Some("clone"),
        Lint::HotAlloc | Lint::HotAllocCall | Lint::StdHashmap => Some("alloc"),
        Lint::HotDyn => Some("dyn"),
        Lint::Parallelizable
        | Lint::SerialDependence
        | Lint::FiberBoundary
        | Lint::ErrorDebug
        | Lint::WallClock
        | Lint::UntypedError
        | Lint::DroppedError
        | Lint::HashOrderOutput
        | Lint::HeldAcrossWait
        | Lint::LockAcrossQuery
        | Lint::BlockingOnPool
        | Lint::UnnamedWait
        | Lint::SharedCollectionLock
        | Lint::OpenWaitLoop => None,
    }
}

/// Whether a finding of `lint` at `range` of `text` is kept by a comment: on one of the lines
/// `range` covers, or on the line above its first.
pub(super) fn exempt(text: &str, range: TextRange, lint: Lint) -> bool {
    let Some(tag) = tag(lint) else { return false };
    let start = (usize::from(range.start())).min(text.len());
    let end = (usize::from(range.end())).clamp(start, text.len());
    let first = line_start(text, start);
    let above = match first {
        0 => 0,
        _ => line_start(text, first - 1),
    };
    let last = text[end..].find('\n').map_or(text.len(), |at| end + at);
    text[above..last].split('\n').any(|line| keeps(line, tag))
}

/// Where the line holding byte `at` starts.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |it| it + 1)
}

/// Whether `line` has `// <tag>: <reason>` with a reason that isn't empty: hot_lint.py's
/// `//\s*(tag):\s*\S`.
fn keeps(line: &str, tag: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find("//") {
        rest = &rest[at + 2..];
        let reason = rest
            .trim_start()
            .strip_prefix(tag)
            .and_then(|it| it.strip_prefix(':'))
            .map(str::trim_start);
        if reason.is_some_and(|it| !it.is_empty()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use syntax::{TextRange, TextSize};

    use super::*;

    fn at(text: &str, needle: &str) -> TextRange {
        let start = text.find(needle).expect("the needle is in the text");
        TextRange::at(TextSize::new(start as u32), TextSize::new(needle.len() as u32))
    }

    #[test]
    fn a_reason_on_the_line_or_above_keeps_it() {
        let text = "fn f() {\n    // clone: setup, once\n    let a = x.clone();\n    let b = y.clone(); // clone: the edge\n    let _ = 0;\n    let c = z.clone();\n}\n";
        assert!(exempt(text, at(text, "x.clone()"), Lint::HotClone));
        assert!(exempt(text, at(text, "y.clone()"), Lint::HotClone));
        assert!(!exempt(text, at(text, "z.clone()"), Lint::HotClone));
    }

    #[test]
    fn an_empty_reason_or_another_tag_does_not() {
        let text = "// clone:\nlet a = x.clone();\n// clone:    \nlet b = y.clone();\n// alloc: why\nlet c = z.clone();\n";
        assert!(!exempt(text, at(text, "x.clone()"), Lint::HotClone));
        assert!(!exempt(text, at(text, "y.clone()"), Lint::HotClone));
        assert!(!exempt(text, at(text, "z.clone()"), Lint::HotClone));
        assert!(exempt(text, at(text, "z.clone()"), Lint::HotAlloc));
        assert!(exempt(text, at(text, "z.clone()"), Lint::StdHashmap));
        assert!(!exempt(text, at(text, "z.clone()"), Lint::HotDyn));
    }

    #[test]
    fn a_doc_comment_is_not_a_reason() {
        let text = "/// dyn: a doc\nd.f();\n";
        assert!(!exempt(text, at(text, "d.f()"), Lint::HotDyn));
        let text = "d.f(); //dyn:x\n";
        assert!(exempt(text, at(text, "d.f()"), Lint::HotDyn));
    }

    #[test]
    fn every_line_of_a_finding_counts() {
        let text = "let v = vec![\n    1, // alloc: setup\n];\nlet w = 0;\n// alloc: below\n";
        assert!(exempt(text, at(text, "vec![\n    1, // alloc: setup\n]"), Lint::HotAlloc));
        assert!(!exempt(text, at(text, "let w = 0;"), Lint::HotAlloc));
        assert!(!exempt("x", TextRange::empty(TextSize::new(0)), Lint::HotAlloc));
    }
}
