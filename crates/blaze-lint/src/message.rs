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
    match finding.template {
        Template::CastToInt => {
            out.str("`").str(range).str("`: a bare cast to `").str(span(0).unwrap_or("?"));
            out.str("` can truncate or wrap; convert with `from`, `try_from` or a named truncation");
        }
        Template::CloneHandle => {
            out.str("clone of a refcounted handle: say so with `.share()` where the type has it");
        }
        Template::CloneErrorPath => {
            out.str("clone on the error path");
        }
        Template::CloneDeep => {
            out.str("clone that copies an allocation: borrow, move or keep one copy");
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
    }
}
