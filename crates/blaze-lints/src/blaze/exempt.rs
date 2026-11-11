//! The comments that keep a cost on a line: `// clone: <why>`, `// alloc: <why>`,
//! `// dyn: <why>`, `// seqcst: <why>`, `// durable: <why>`, `// wakes: <who>` and
//! `// identity: <why>`, on a line of the finding or on the line above it, with a reason that isn't
//! empty. Read from the text of the tree walked, nothing allocated.

use syntax::TextRange;

use super::Lint;

/// The word of the comment that keeps `lint`'s cost; none for the parallel lints, which say
/// what a loop could be rather than what a line costs.
const fn tag(lint: Lint) -> Option<&'static str> {
    match lint {
        Lint::HotClone => Some("clone"),
        Lint::HotAlloc | Lint::HotAllocCall | Lint::ImplicitAllocator => Some("alloc"),
        Lint::HotDyn => Some("dyn"),
        Lint::SeqcstUnexplained => Some("seqcst"),
        Lint::DurableWrite => Some("durable"),
        Lint::UnboundedWait => Some("wakes"),
        Lint::CombinedIdentity => Some("identity"),
        // `relaxed_publish` reads it on either side and on the atomic's declaration itself.
        Lint::RelaxedPublish => Some("relaxed"),
        Lint::Parallelizable
        | Lint::SerialDependence
        | Lint::FiberBoundary
        | Lint::ErrorDebug
        | Lint::WallClock
        | Lint::UntypedError
        | Lint::DroppedError
        | Lint::HashOrderOutput
        | Lint::HeldAcrossWait
        // Kept with `#[expect(..)]`: where a function reads a thread-local is its own choice.
        | Lint::ThreadLocalInWaitingFn
        | Lint::LockAcrossQuery
        | Lint::LockHeldAcrossSlow
        | Lint::BlockingOnPool
        | Lint::UnnamedWait
        | Lint::SharedCollectionLock
        | Lint::OpenWaitLoop
        | Lint::UnrecordedSpan
        // Kept with `#[expect(blaze::fixed_key_lookup, reason = ..)]`: a table is a choice of
        // the item, not of one line.
        | Lint::FixedKeyLookup
        // Kept with `#[expect(..)]`: a literal's base and a signature's `bool` are choices of
        // the item, not of one line.
        | Lint::StructUpdate
        | Lint::BoolParam
        | Lint::ManyBools
        // Kept with `#[expect(..)]`: a core and a key are choices of the item, not of one line
        // (`key_ignores_field`'s `// not in key: <why>` names one field, through [`commented`]).
        | Lint::SansIoCore
        | Lint::KeyIgnoresField
        // Kept with `#[expect(blaze::wildcard_arm, reason = ..)]` on the arm, the `match` or
        // the item.
        | Lint::WildcardArm
        // Kept with `#[expect(..)]`: a struct's layout, a signature's type and a collection
        // are choices of the item, not of one line.
        | Lint::FalseSharing
        | Lint::LargeMove
        | Lint::CollectThenConsume
        // Kept with `#[expect(..)]` on the item: a map's key type and a loop's shape are not
        // one line's choice.
        | Lint::HashTwice
        | Lint::SyscallInLoop
        | Lint::QueryInLoop
        | Lint::StringKey
        // Kept with `#[expect(..)]` on the item: a function's shape, a signature's error and a
        // field's type are the item's choice, not one line's.
        | Lint::OutlineGenericBody
        | Lint::ColdError
        | Lint::FrozenCollection
        // Kept with `#[expect(..)]` on the item: a type's representation, a map's shape and a
        // binding's ownership are the item's choice, not one line's.
        | Lint::InternCandidate
        | Lint::SmallMap
        | Lint::ArcCloneOnlyBorrowed
        // Kept with `#[expect(..)]` on the item: a blocking call, an ABI, a loop's cap, a map's
        // hasher and a `drop` are the item's choice, not one line's.
        | Lint::BlockingInAsync
        | Lint::UnwindAcrossFfi
        | Lint::UnboundedFromInput
        | Lint::DosHasherOnInput
        | Lint::PanicInDrop
        // Kept with `#[expect(..)]` on the item: a loop's indexing, a stream's buffering, a
        // collection's type, an iterator's hint, a sort and a field's type are the item's choice.
        | Lint::BoundsCheckInLoop
        | Lint::UnbufferedIo
        | Lint::StdoutLockInLoop
        | Lint::FrontVecOps
        | Lint::MissingSizeHint
        | Lint::FmtAllocates
        | Lint::JaggedVec
        | Lint::StableSortNeedless
        | Lint::DynInHotField
        // A profile's word on a function: no comment changes what it counted.
        | Lint::ProfileHot
        | Lint::StaleHotMarker
        | Lint::HotMarkedCold
        | Lint::PushWithoutReserve
        | Lint::ScratchInLoop
        | Lint::BoxedRecursiveNode
        | Lint::WideIndex
        | Lint::BoundedHeapField
        | Lint::EnumVecPadded
        | Lint::DeepDropAtExit
        | Lint::DynFnParam
        | Lint::BoxLargeNew
        | Lint::PodSerializedByField
        | Lint::Checkfree
        | Lint::Nopanic
        | Lint::Noalloc
        | Lint::Nonblocking
        | Lint::Pure
        // Kept by the list itself, or with `#[expect(..)]` on the function.
        | Lint::UndeclaredEffect
        | Lint::UnneededEffect
        | Lint::NegativeEffect
        | Lint::Packable
        | Lint::SoaCandidate
        | Lint::BorrowWorkaround
        | Lint::NllWorkaround
        | Lint::NativeContracts
        // Kept with `#[expect(..)]` on the item: what a format accepts is the type's choice.
        | Lint::LenientDeserialize
        // Kept with `#[expect(..)]` on the macro's function or the script: a macro and a
        // script's reach are the item's choice, not one line's.
        | Lint::ProcMacroDerive
        | Lint::ProcMacroAttribute
        | Lint::ProcMacroFnLike
        | Lint::BuildScriptAmbient
        // Kept with `#[expect(..)]` on the function: what a function reaches is its choice.
        | Lint::StdUse
        // A hasher is chosen in the type, or kept with `#[expect(blaze::default_hasher, reason = ..)]`:
        // an allocation's comment says nothing about it.
        | Lint::DefaultHasher => None,
    }
}

/// Whether a finding of `lint` at `range` of `text` is kept by a comment: on one of the lines
/// `range` covers, or on the line above its first.
pub(super) fn exempt(text: &str, range: TextRange, lint: Lint) -> bool {
    let Some(tag) = tag(lint) else { return false };
    commented(text, range, tag)
}

/// Whether `// <tag>: <reason>` is on one of the lines `range` of `text` covers, or on the line
/// above its first, with a reason that isn't empty.
pub(super) fn commented(text: &str, range: TextRange, tag: &str) -> bool {
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
        assert!(!exempt(text, at(text, "z.clone()"), Lint::DefaultHasher));
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
