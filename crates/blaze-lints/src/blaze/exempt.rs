//! The comments that keep a cost on a line: `// clone: <why>`,
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
        Lint::HotDyn => Some("dyn"),
        Lint::SeqcstUnexplained => Some("seqcst"),
        Lint::DurableWrite => Some("durable"),
        Lint::UnboundedWait => Some("wakes"),
        Lint::CombinedIdentity => Some("identity"),
        // `relaxed_publish` reads it on either side and on the atomic's declaration itself.
        Lint::RelaxedPublish => Some("relaxed"),
        // No comment stands in for an allocator (rule bump-allocate): the global heap is named
        // where it is used (`Vec::new_in(Global)`), or the item says why with `#[expect(..)]`,
        // which the ratchet counts. `// alloc: <why>` keeps nothing; the lines that still carry
        // it are what `scripts/src/bin/allocs.rs` counts down to zero.
        Lint::HotAlloc
        | Lint::HotAllocCall
        | Lint::ImplicitAllocator
        | Lint::Parallelizable
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
        // Kept with `#[expect(..)]` on the function: what a turn, an answer or a lock order may
        // reach is the function's choice, not one line's.
        | Lint::LoopTurnEffects
        | Lint::SyncOnAnswerPath
        | Lint::LockOrder
        | Lint::LockAcrossWait
        | Lint::LockHeldAcrossSlow
        | Lint::BlockingOnPool
        | Lint::UnnamedWait
        | Lint::SharedCollectionLock
        | Lint::OpenWaitLoop
        | Lint::UnrecordedSpan
        // No comment makes the profile keep a field: the fix is the field's name.
        | Lint::SpanFieldUnrecorded
        // Kept with `#[expect(blaze::fixed_key_lookup, reason = ..)]`: a table is a choice of
        // the item, not of one line.
        | Lint::FixedKeyLookup
        // Kept with `#[expect(..)]`: a literal's base and a signature's `bool` are choices of
        // the item, not of one line.
        | Lint::StructUpdate
        | Lint::BoolParam
        | Lint::ManyBools
        | Lint::DefaultPolicy
        // Kept with `#[expect(..)]`: a core and a key are choices of the item, not of one line
        // (`key_ignores_field`'s `// left out: <why>` names one field, through [`commented`]).
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
        | Lint::Unreachable
        | Lint::Todo
        | Lint::Noalloc
        | Lint::Nonblocking
        | Lint::Pure
        | Lint::Capabilities
        // Kept by the list itself, or with `#[expect(..)]` on the function.
        | Lint::UndeclaredEffect
        | Lint::UnfulfilledEffect
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
        // About the package: allowed in the workspace's lint table or at the crate root.
        | Lint::NotLatestEdition
        // Kept with `#[expect(..)]` on the function: what a function reaches is its choice.
        | Lint::StdUse
        // A hasher is chosen in the type, or kept with `#[expect(blaze::default_hasher, reason = ..)]`:
        // an allocation's comment says nothing about it.
        | Lint::DefaultHasher
        // The edition's lints name a construct the edition removes, not a cost on a line.
        | Lint::AsCast
        | Lint::ImplicitOverflow
        | Lint::GlobalState
        | Lint::AsyncFn
        | Lint::MutRef
        // Kept with `#[expect(..)]` on the function: a mark is the item's choice, not a line's.
        | Lint::UnmarkedPure
        | Lint::UnmarkedNopanic
        | Lint::UnmarkedNoalloc
        | Lint::UnmarkedNonblocking
        // A type's declaration and where it is held: kept with `#[expect(..)]` on the item.
        | Lint::ProgramLifetime
        | Lint::CouldBeProgramLifetime
        | Lint::CouldBeRef32
        // Suggestions and the refinement gate: advice is taken or left, never kept by a comment.
        | Lint::CouldBeNarrower
        | Lint::CouldBeSansIo
        | Lint::CouldBeLinear
        | Lint::CouldHaveInvariant
        | Lint::Refinement
        // Kept with `#[expect(..)]` on the function: where a setting is read is the item's
        // choice, and the allowed modules are named in `clippy.toml`.
        | Lint::EnvRead
        // Kept with `#[expect(..)]` on the item: a child's owner, a test's bound, a reader of
        // positions (or `#[blaze::renders_positions]`) and a format's field order are the
        // item's choice. In a test the code under test is the waker, so `// wakes:` keeps no
        // `unbounded_test_wait` finding.
        | Lint::UntiedChild
        | Lint::UnboundedTestWait
        | Lint::ReadsPositions
        | Lint::AbsolutePosition
        | Lint::CborFieldOrder
        // Kept with `#[expect(..)]` on the item: a literal's alignment, a count, a silenced
        // design, a policy path, a bounded length and a bounded static are the item's choice.
        | Lint::GappedLiteral
        | Lint::Retry
        | Lint::Unwired
        | Lint::TmpPath
        | Lint::NarrowedLen
        | Lint::ProcessStatic
        // Kept with `#[expect(..)]` on the item: whether wrapping is meant is the code's
        // choice, said once with its reason.
        | Lint::TruncatingCast
        // The std mistakes: each place that is right says why with `#[expect(..)]`.
        | Lint::PartialWrite
        | Lint::PartialCmpUnwrap
        | Lint::UnboundedRead
        | Lint::LenMinus
        | Lint::StrByteSlice
        | Lint::Forget
        | Lint::PathToStrUnwrap
        | Lint::DetachedThread
        | Lint::UnboundedChannel
        | Lint::Transmute
        | Lint::CloneToSatisfy
        | Lint::UncheckedIndexInLoop
        | Lint::DefaultNumericFallbackLen
        | Lint::RefcellBorrowAcrossCall
        | Lint::ProcessExit
        | Lint::PrintInLib
        | Lint::LockUnwrap
        | Lint::SkippedPair
        | Lint::HalfDone
        | Lint::HiddenExit
        | Lint::QuestionMarkConverts
        | Lint::OptionQuestionMarkHides
        | Lint::UnseenTarget
        | Lint::GlobImport
        | Lint::ShadowChangesMeaning
        | Lint::VersionedName
        | Lint::UnusedParameter
        | Lint::Fallback
        | Lint::PlaceholderBody
        | Lint::TestAssertsNothing
        | Lint::AssertionCannotFail
        | Lint::TestMissesItsSubject
        | Lint::IgnoredWithoutReason
        | Lint::DocNamesNothing
        | Lint::MagicNumber
        | Lint::MagicString
        | Lint::RepeatedFormat
        | Lint::StringSharedAcrossCrates
        | Lint::StringNearMissAcrossCrates
        | Lint::SwappedArguments
        | Lint::WriteOnlyField
        | Lint::OnlyUsedInTests
        | Lint::SingleImplTrait
        | Lint::ForwardingFn
        | Lint::ConstantArgument
        | Lint::AlwaysSameResult
        | Lint::NeverConstructed
        | Lint::NeverMatched
        | Lint::SwappableParameters
        // Held by the per-item baseline (`baselines/items.tsv`), never by a comment.
        | Lint::FunctionWeight
        | Lint::PathCount
        | Lint::LiveState
        | Lint::EffectBreadth
        | Lint::Exits
        | Lint::LongFunction
        | Lint::LongFile
        | Lint::ManyParams
        | Lint::WideType
        | Lint::DeepNesting
        | Lint::FanOut
        | Lint::ModuleCycle
        | Lint::ProofForgotten
        | Lint::UnitMismatch
        | Lint::PrimitiveMeaning
        | Lint::SentinelValue
        | Lint::StateInFlags
        | Lint::BoolPairState
        | Lint::OutParameter
        | Lint::ParallelCollections
        | Lint::UntypedIndex
        | Lint::ValidatedButRaw
        | Lint::ClaimContradictsFacts
        | Lint::ClaimIsDerivable
        | Lint::ClaimWithoutEvidence
        | Lint::EvidenceIsStale
        | Lint::SafetyWithoutInvariant
        | Lint::CommentClaims
        // Held by `#[expect(blaze::<lint>, reason = ..)]` and the ratchet baselines.
        | Lint::SameFn
        | Lint::SameImplFn
        | Lint::SameBlock
        | Lint::NearDuplicate
        | Lint::PasteMismatch
        | Lint::SameType
        | Lint::SameConst
        | Lint::ExtractableDuplicate
        | Lint::TestsDifferByLiteral
        | Lint::ImplsDifferByType
        | Lint::DuplicateAcrossCrates
        // Held by `#[expect(blaze::<lint>, reason = ..)]` on the function or the test.
        | Lint::ResultPartUnobserved
        | Lint::WeakAssertion
        | Lint::MirrorAssertion
        | Lint::PseudoTested
        | Lint::UnreachedByTests
        | Lint::ErrorPathUntested
        | Lint::PropertyOverExamples
        // Kept with `#[expect(..)]` on the impl, the enum or the list: a field left out of a
        // field-wise body says so with `// left out: <why>` on its `field: _`, through
        // [`commented`], not by a tag.
        | Lint::HandWrittenDerivable
        | Lint::ImplMissesField
        | Lint::HandKeptVariantList
        | Lint::VariantListDrift
        | Lint::HandWrittenCodec
        | Lint::FieldWiseConversion
        | Lint::ConversionMissesField
        | Lint::HandWrittenVisitor
        | Lint::RepeatedFieldList
        | Lint::RegistrySpread
        // Held by `#[expect(blaze::same_behaviour, reason = ..)]` and the ratchet baseline.
        | Lint::SameBehaviour
        // About a dependency: granted in the root manifest's `[workspace.metadata.blaze.trust]`,
        // or for a workspace crate's own `unsafe` at its root, never by a comment on a line.
        | Lint::DependencyCapability
        | Lint::DependencyUnsafe
        | Lint::TrustGrantUnused
        | Lint::DependencyCapabilityUnreached
        | Lint::WorkspaceUnsafe
        // Off by default; held by `#[expect(blaze::loop_match_candidate, reason = ..)]`.
        | Lint::LoopMatchCandidate => None,
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
        let text = "// clone:\nlet a = x.clone();\n// clone:    \nlet b = y.clone();\n// dyn: why\nlet c = z.clone();\n";
        assert!(!exempt(text, at(text, "x.clone()"), Lint::HotClone));
        assert!(!exempt(text, at(text, "y.clone()"), Lint::HotClone));
        assert!(!exempt(text, at(text, "z.clone()"), Lint::HotClone));
        assert!(exempt(text, at(text, "z.clone()"), Lint::HotDyn));
        assert!(!exempt(text, at(text, "z.clone()"), Lint::DefaultHasher));
        // No comment stands in for an allocator (rule bump-allocate): `// alloc:` keeps nothing.
        let text = "// alloc: why\nlet v = Vec::new();\n";
        assert!(!exempt(text, at(text, "Vec::new()"), Lint::HotAlloc));
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
        let text = "let v = f(\n    1, // dyn: setup\n);\nlet w = 0;\n// dyn: below\n";
        assert!(exempt(text, at(text, "f(\n    1, // dyn: setup\n)"), Lint::HotDyn));
        assert!(!exempt(text, at(text, "let w = 0;"), Lint::HotDyn));
        assert!(!exempt("x", TextRange::empty(TextSize::new(0)), Lint::HotDyn));
    }

    /// The rule: no comment stands in for an allocator. `// alloc: <why>`, with a reason,
    /// keeps no finding of `implicit_allocator`, `hot_alloc` or `hot_alloc_call`.
    #[test]
    fn an_alloc_comment_keeps_no_allocation_finding() {
        let text = "// alloc: handed to the caller\nlet v = Vec::new();\nlet w = Vec::new(); // alloc: setup\n";
        for lint in [Lint::ImplicitAllocator, Lint::HotAlloc, Lint::HotAllocCall] {
            assert!(!exempt(text, at(text, "Vec::new();\nlet"), lint));
            assert!(!exempt(text, at(text, "let w = Vec::new();"), lint));
        }
    }
}
