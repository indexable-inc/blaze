//! A finding's message, written from its template and arguments at print time: the words are the
//! template's, the arguments a range of the item's text or a row of the rule set's tables.

use blaze_text::{Put, Writes};

use crate::{Arg, Finding, Findings, RuleSet, Template};

/// Writes `finding`'s message to `out`. `item` is the text of the item it was found in (its
/// ranges are relative to it); `rules` the rule set it was found by (its tables' rows).
pub fn show_message<P: Put + ?Sized>(rules: &RuleSet, item: &str, findings: &Findings<'_>, finding: &Finding, out: &mut P) {
    let args = findings.args_of(finding);
    let span = |at: usize| match args.get(at) {
        Some(Arg::Span(span)) if span.end as usize <= item.len() && span.start <= span.end => Some(span.of(item)),
        _ => None,
    };
    let row = |at: usize| match args.get(at) {
        Some(Arg::Int(row)) => rules.renames().get(*row as usize),
        _ => None,
    };
    let range = finding.range.of(item);
    // A sequence's row: what to write instead of it.
    let instead = || match args.first() {
        Some(Arg::Int(row)) => crate::sequences::SEQS.get(*row as usize).map_or("?", |seq| seq.instead),
        _ => "?",
    };
    match finding.template {
        Template::CastToInt => {
            out.str("`").str(range).str("`: a bare cast to `").str(span(0).unwrap_or("?"));
            out.str("` can truncate or wrap; convert with `from`, `try_from` or a named truncation");
        }
        Template::CloneHandle => {
            out.str("clone of a refcounted handle: say so with `.local_share()` (for `Rc`) or `.share()` (for `Arc`)");
        }
        Template::CloneErrorPath => {
            out.str("clone on the error path");
        }
        Template::CloneDeep => {
            out.str("clone that copies an allocation: borrow, move, or name the allocator with `.clone_in(alloc)`");
        }
        Template::CloneUnknown => {
            out.str("clone of unknown kind: say what it copies with `// clone: <why>` above it");
        }
        Template::AllocGlobal => {
            out.str("`Global` names the global heap: take the caller's allocator");
        }
        Template::AllocSignature => {
            out.str("signature fixes its caller's allocator with `").str(span(0).unwrap_or("?"));
            out.str("`: take an allocator parameter");
        }
        Template::AllocComment => {
            out.str("an `// alloc:` hatch: name the allocator in code instead");
        }
        Template::AllocExpect => {
            out.str("`blaze::implicit_allocator` allowed here: name the allocator instead");
        }
        Template::UntypedString | Template::UntypedStr | Template::UntypedBoxed | Template::UntypedAnyhow => {
            out.str("untyped error `").str(span(0).unwrap_or("?"));
            out.str("`: return an error type naming what failed and where");
        }
        Template::UntypedAlias => {
            out.str("alias of a result with an untyped error: `").str(span(0).unwrap_or("?")).str("`");
        }
        Template::Recursion => {
            out.str("`").str(range).str("` calls itself (at `").str(span(0).unwrap_or("?"));
            out.str("`): use a loop over an explicit stack, or mark another impl's call");
        }
        Template::StopUnwrap | Template::StopExpect => {
            out.str("`").str(range).str("` stops the program: return a typed error");
        }
        Template::StopPanic | Template::StopUnreachable | Template::StopTodo | Template::StopUnimplemented => {
            out.str("`").str(range).str("` stops the program: return a typed error");
        }
        Template::StopIndex => {
            out.str("index `").str(range).str("` can be out of bounds: use `get` or a checked length");
        }
        Template::AmbientStd | Template::AmbientAlloc => {
            out.str("`").str(range).str("`");
            if let Some(segment) = span(0) {
                out.str("::").str(segment);
            }
            out.str(" is ambient: use the bstd or blaze-own form");
        }
        Template::AmbientCollection => {
            out.str("`").str(range).str("` allocates in the global heap: use blaze-own's, in a named allocator");
        }
        Template::UnsafeNoComment => {
            out.str("`unsafe` with no `// SAFETY:` comment above it stating what it assumes");
        }
        Template::FormatMacro => {
            out.str("`").str(span(0).unwrap_or(range)).str("!` outside tests: write with blaze-text's `Writes`");
        }
        Template::RenamedFixed | Template::RenamedReport => match row(0) {
            Some(rename) => {
                out.str("`").str(rename.old).str("` was renamed `").str(rename.new).str("`");
                if !rename.suffix.is_empty() {
                    out.str(" (then `").str(rename.suffix).str("`)");
                }
                if finding.template == Template::RenamedReport {
                    out.str(": the receiver's type is not known here, so it is not fixed");
                }
            }
            None => {
                out.str("`").str(range).str("` was renamed");
            }
        },
        Template::HeapReach => {
            out.str("`").str(range).str("` reaches for the process's heap: take ").str(instead());
        }
        Template::HeapConstructor | Template::HeapMacro | Template::HeapMethod => {
            // Text cannot see which `Vec` or which receiver this is: the finding says so.
            out.str("`").str(range).str("` allocates in the global heap if it is std's: use ").str(instead());
        }
        Template::Async => {
            out.str("`async` code: write the explicit state machine (blaze's own code is sans-IO state machines)");
        }
        Template::Await => {
            out.str("`.await`: write the explicit state machine (blaze's own code is sans-IO state machines)");
        }
        Template::ReadClock
        | Template::ReadEnv
        | Template::ReadRandom
        | Template::ReadHashSeed
        | Template::ReadProcess => {
            out.str("`").str(range).str("` reads the process's state where nothing passed it in: use ").str(instead());
        }
        Template::UndoneTodo | Template::UndoneUnimplemented => {
            out.str("`").str(range).str("` is work left undone");
        }
        Template::UndoneDbg => {
            out.str("`").str(range).str("` is a debugging print left in");
        }
        Template::UndoneUnreachable => {
            out.str("`unreachable!()` with no message: say why it cannot be reached");
        }
        Template::WaitCall => {
            out.str("`").str(range).str("` waits with no deadline: use ").str(instead());
        }
        Template::WaitLoop => {
            out.str("`loop` with no `break`, `return` or `?`: bound it (a count, a deadline) or say what ends it");
        }
        Template::HasherStd => {
            out.str("`").str(range).str("` is std's or hashbrown's table or hasher, its hasher not written: use ").str(instead());
        }
        Template::HasherDefault => {
            out.str("`").str(range).str("` builds its hasher from `Default` (D125): write it, ").str(instead());
        }
        Template::StaticThreadLocal | Template::StaticMut => {
            out.str("`").str(range).str("` is state the process keeps with no owner (D170 C3): pass it in as a value its caller owns");
        }
        Template::StaticState => {
            out.str("`").str(range).str("` holds `").str(span(0).unwrap_or("?"));
            out.str("`, state the process keeps with no owner (D170 C3): pass it in as a value its caller owns");
        }
        Template::SansIoPath => {
            out.str("`").str(range).str("` does IO in a sans-IO core (if it is std's or the system's): return it as the ");
            out.str("machine's output and let the driver (`#![blaze::driver]`) do it");
        }
        Template::SansIoBlocking => {
            out.str("`").str(range).str("` blocks the thread in a sans-IO core: return what is waited for as the machine's ");
            out.str("output and take its answer as an input");
        }
        Template::SansIoClock => {
            out.str("`").str(range).str("` reads the wall clock in a sans-IO core: take the time as an input");
        }
        Template::SansIoPrint => {
            out.str("`").str(range).str("` prints in a sans-IO core: return what to say as the machine's output");
        }
        Template::LockNoReason => {
            // The lock's name (its first argument's string), else the constructor's path.
            out.str("`").str(span(0).unwrap_or(range)).str("`: a lock with no reason: say which D179 shape it could not be");
            out.str(" (`#[blaze::lock_exception(tried = \"published\"|\"gathered\"|\"claimed\", why = \"..\")]`)");
        }
        Template::LockBadReason => {
            out.str("`").str(range).str("`: a lock's reason says `tried = \"published\"`, `\"gathered\"` or `\"claimed\"` ");
            out.str("and a `why` that is not empty, each once, and nothing else (D179)");
        }
        Template::WorkspaceUnsafe => {
            out.str("`unsafe` in a crate without `#![blaze::trust(unsafe, reason = \"..\")]` at its root");
        }
        Template::SeqCstUnexplained => {
            out.str("`Ordering::SeqCst` with no `// seqcst: <why>` comment saying why one total order is needed");
        }
        Template::RawSync => {
            out.str("`").str(range).str("` is a raw synchronization primitive: use ").str(instead());
        }
        Template::BlockingOnPool => {
            out.str("`").str(range).str("` blocks or spawns an OS thread on the pool: use ").str(instead());
        }
        Template::Transmute => {
            out.str("`").str(range).str("` reinterprets raw bits with only a size check: use ").str(instead());
        }
        Template::BoolParam => {
            out.str("`").str(span(0).unwrap_or(range)).str(": bool` in a public signature: use a two-variant enum or a `#[blaze::args]` options struct so call sites name the choice");
        }
        Template::SwappableParameters => {
            out.str("`").str(range).str("` has adjacent `").str(span(0).unwrap_or("?"));
            out.str("` parameters that can be silently transposed: wrap them in distinct newtypes or use a `#[blaze::args]` options struct");
        }
        Template::PrimitiveMeaning => {
            out.str("`").str(range).str("` has bare `").str(span(0).unwrap_or("?"));
            out.str("` carrying a unit or entity identity in its name: wrap it in a zero-cost newtype");
        }
        Template::AnonTupleReturn => {
            out.str("`").str(range).str("` returns an anonymous tuple with unnamed state (`").str(span(0).unwrap_or("?"));
            out.str("`): define a named struct so each field carries its meaning");
        }
        Template::HandWrittenDiscriminant => {
            out.str("`").str(range).str("` hand-maps enum variants to integer tags: use `#[repr(u8)]`, `core::mem::discriminant`, or `bstd::comptime` derive instead");
        }
        Template::ProcMacroDef => {
            out.str("`").str(range).str("` defines a proc macro: under Blaze, prefer `bstd::comptime` reflection (`#[cfg(blaze)] comptime fn`) to avoid proc-macro build overhead");
        }
        Template::DoubleIndirectionArc => {
            let replacement = match span(0) {
                Some("String") => "`blaze_own::ThinStr`",
                Some("Vec") => "`blaze_own::ThinArc`",
                _ => "`blaze_own::ThinArc` / `blaze_own::ThinStr`",
            };
            out.str("`").str(range).str("` causes double heap indirection: use ").str(replacement);
            out.str(" (single 8-byte thin pointer + single allocation)");
        }
        Template::RawSandboxExec => {
            out.str("raw ").str(range).str(" process spawn outside `bstd::procs::hermetic`: use `bstd::procs::hermetic::Hermetic` for cross-platform macOS Seatbelt and Linux Landlock/seccomp from one sans-IO `Plan`");
        }
        Template::MutRef => {
            out.str("`").str(range).str("` is deprecated in Blaze: spell unique references `&uniq`");
        }
        Template::RefMut => {
            out.str("`").str(range).str("` is deprecated in Blaze: spell unique reference bindings `ref uniq`");
        }
        Template::MagicSlicePrefix => {
            let rhs = span(0).unwrap_or("..");
            out.str("magic slice range `").str(range).str("` hardcodes the length of `").str(rhs)
                .str("`: derive it with `strip_prefix(").str(rhs).str(")` (to also advance past the prefix) or `starts_with(")
                .str(rhs).str(")`");
        }
        Template::MagicSlicePrefixMismatch => {
            let rhs = span(0).unwrap_or("..");
            out.str("magic slice range `").str(range).str("` has a hardcoded length that does not match `").str(rhs)
                .str("` (this comparison can never succeed): use `strip_prefix(").str(rhs).str(")` or `starts_with(")
                .str(rhs).str(")` so the length is derived");
        }
        Template::MagicSliceAdvance => {
            let prefix = span(0).unwrap_or("..");
            out.str("hardcoded advance `").str(range).str("` after `starts_with(").str(prefix)
                .str(")`: derive the remainder directly with `if let Some(rest) = ..strip_prefix(").str(prefix).str(")`");
        }
        Template::MagicSliceBytes => {
            out.str("magic byte-slice length in `").str(range)
                .str("`: derive it from `core::mem::size_of::<T>()`, a named `const`, or use `split_first_chunk` / a typed reader");
        }
        Template::MagicSliceRange => {
            out.str("magic slice range `").str(range)
                .str("`: derive the length from `strip_prefix`, `.len()`, or `size_of::<T>()`, or bind a named `const` instead of hardcoding a literal");
        }
        Template::BoolAsInt => {
            let ty = span(0).unwrap_or("usize");
            out.str("boolean-to-integer cast `").str(range).str("`: use infallible `").str(ty).str("::from(..)`");
        }
        Template::LenZeroCheck => {
            out.str("manual emptiness check `").str(range).str("`: use `.is_empty()`");
        }
        Template::IndexLoop => {
            let slice = span(0).unwrap_or("slice");
            out.str("index loop `").str(range).str("` indexes `").str(slice)
                .str("[..]` directly (unchecked stop + bounds check): iterate with `for x in &").str(slice)
                .str("` or `").str(slice).str(".iter().enumerate()`");
        }
        Template::ManualFindSplit => {
            let pat = span(0).unwrap_or("pat");
            out.str("manual slice split after `.find(").str(pat)
                .str(")` at `").str(range).str("`: use `.split_once(").str(pat).str(")` to derive both halves without manual offsets");
        }
        Template::TestGlobalAlloc => {
            out.str("`Alloc::global()` inside test code trips `alloc_global_tests`: use `blaze_own::testing::heap().alloc()`");
        }
        Template::TestExternStd => {
            out.str("`extern crate std;` in test code trips `std-alloc-tests`: keep tests `#![no_std]` with `blaze_own::testing::heap().alloc()` and `core::fmt`");
        }
        Template::MutForUniqBorrow => {
            out.str("`").str(range).str("` is never rebound and only uniquely borrowed: under Blaze's edition `let` allows `&uniq` directly and `mut` is only for local rebinding");
        }
        Template::IoRef => {
            out.str("`").str(range).str("` borrows the general `Io` capability bundle: take only the narrow capability needed (`&Out`, `&Fs`, `&Clock`, `&Host`, `&Args`, `CxSpawn`, ...)");
        }
        Template::UnusedImport => {
            out.str("unused or non-minimal import `").str(span(0).unwrap_or(range)).str("`: remove unused entries and redundant braces (`blaze check --fix`)");
        }
        Template::UnusedMut => {
            out.str("variable `").str(span(0).unwrap_or(range)).str("` does not need to be mutable: remove `mut` (`blaze check --fix`)");
        }
        Template::HeapLeak => {
            out.str("`").str(range).str("` leaks allocations: for bump arenas use `.into_bump_slice()` / `.into_bump_str()` / `frame.alloc_str()`, or `ProgramArena` for intentional `'static` state");
        }
        Template::VariantCount | Template::VariantCountMany => {
            let names = || {
                args.iter().filter_map(|arg| match arg {
                    Arg::Text(run) => Some(findings.text_in(*run)),
                    _ => None,
                })
            };
            out.str("`").str(range).str("` is the number of variants of ");
            for (at, name) in names().enumerate() {
                out.str(if at == 0 { "`" } else { "`, `" }).str(name);
            }
            out.str("`");
            if finding.template == Template::VariantCountMany {
                out.str(": if it counts one of them, write `core::mem::variant_count::<E>()` with that enum (feature ");
                out.str("`variant_count`) so a new variant cannot leave it stale");
                return;
            }
            let name = names().next().unwrap_or("?");
            out.str(": write `core::mem::variant_count::<").str(name).str(">()` (feature `variant_count`) so a new variant cannot leave it stale");
            match args.first() {
                Some(Arg::Int(crate::variants::ELSEWHERE)) => {
                    out.str(" (not fixed: `").str(name).str("` is in another module or a block, so name its path)");
                }
                Some(Arg::Int(crate::variants::GENERIC)) => {
                    out.str(" (not fixed: `").str(name).str("` is generic, so name its parameters)");
                }
                Some(Arg::Int(crate::variants::UNTYPED)) => {
                    out.str(" (not fixed: the type here is not known to be `usize`, which the call returns)");
                }
                _ => {}
            }
        }
    }
}
