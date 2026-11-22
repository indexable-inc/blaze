//! `format_args!` (and `format_args_nl!`, `const_format_args!`) as one node with its template
//! parsed, as rustc's `rustc_builtin_macros::format` makes `ast::FormatArgs`: the template's
//! pieces, the written arguments (each parsed as an expression by the same parser and lowered
//! by the same machine, in place), and the arguments the template captures (`{x}`, `{:w$}`),
//! each a path expression resolved here when it names a local.
//!
//! What stays for the checker: unused arguments, an index past the arguments, and the
//! format specs' meaning (kept as ranges).

use blaze_own::{AVec, Vec};
use blaze_surface::Range;
use parser::{LexedStr, OutlineStep, PrefixEntryPoint, Step, SyntaxKind};

use crate::form::*;
use crate::machine::{Machine, V, idx};

/// The byte offsets of a string token's contents in `text`: `"..."` and `r#"..."#`.
pub(crate) fn string_contents(text: &str) -> Option<(usize, usize)> {
    if let Some(raw) = text.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let start = 1 + hashes + 1;
        let end = text.len().checked_sub(hashes + 1)?;
        return (start <= end).then_some((start, end));
    }
    let end = text.len().checked_sub(1)?;
    (text.starts_with('"') && text.ends_with('"') && end >= 1).then_some((1, end))
}

/// A template literal's VALUE, the string a format string is (the Reference, "String
/// literals": a non-raw literal's escapes applied, a raw literal's value its text), with the
/// source offset of each value byte. rustc parses the value
/// (rustc_builtin_macros format.rs, `rustc_parse_format::Parser`) and maps it back to the
/// snippet only for spans. So `\u{e9}` is one char, and `\x7b` is a brace.
pub(crate) struct TemplateValue<'a> {
    pub(crate) value: blaze_own::AString<'a>,
    /// `src[k]`: the offset, in the literal's contents, of the source char value byte `k` came
    /// from; `src[value.len()]` is the contents' length.
    pub(crate) src: AVec<'a, u32>,
    /// The first escape the literal's rules refuse, by its range in the contents (its value
    /// is left out of `value`).
    pub(crate) bad: Option<core::ops::Range<usize>>,
}

/// [`TemplateValue`] of a string literal's `contents` (between its quotes); `raw` for
/// `r".."` and `r#".."#`.
pub(crate) fn template_value<'a>(contents: &str, raw: bool, alloc: blaze_own::Alloc<'a>) -> TemplateValue<'a> {
    let mut value = blaze_own::AString::new_in(alloc);
    let mut src: AVec<'a, u32> = Vec::new_in(alloc);
    let mut bad = None;
    let mut each = |r: core::ops::Range<usize>, c: Result<char, parser::unescape::EscapeError>| match c {
        Ok(c) => {
            for _ in 0..c.len_utf8() {
                src.push(idx(r.start));
            }
            value.push(c);
        }
        Err(e) if e.is_fatal() && bad.is_none() => bad = Some(r),
        // A warning (a skipped line's whitespace) has no value; a second refusal adds nothing.
        Err(_) => {}
    };
    if raw {
        parser::unescape::check_raw_str(contents, &mut each);
    } else {
        parser::unescape::unescape_str(contents, &mut each);
    }
    src.push(idx(contents.len()));
    TemplateValue { value, src, bad }
}

/// The first token at or after `from` that is not trivia, in `lexed`.
fn non_trivia(lexed: &LexedStr<'_>, from: usize) -> Option<usize> {
    (from..lexed.len()).find(|&i| !lexed.kind(i).is_trivia() && lexed.kind(i) != SyntaxKind::EOF)
}

/// How many lexer tokens a parse's steps took.
fn consumed(output: &parser::Output) -> usize {
    let mut n = 0usize;
    for step in output.steps() {
        match step {
            OutlineStep::Step(Step::Token { n_input_tokens, .. }) => n += usize::from(n_input_tokens),
            OutlineStep::Step(Step::FloatSplit { .. }) => n += 1,
            OutlineStep::Lazy { n_input_tokens, .. } => n += n_input_tokens as usize,
            OutlineStep::Step(_) => {}
        }
    }
    n
}

/// One written argument: its name with `name = expr`, its expression and range.
#[derive(Clone, Copy)]
struct Written {
    name: Option<NameIdx>,
    expr: ExprId,
    range: Range,
}

impl Machine<'_, '_, '_> {
    /// Lowers `builtin # format_args(template, args..)`, the form the expander's
    /// `format_args!` gives back once its template is expanded: the same node as
    /// `format_args!(..)`. Its template and arguments are already lowered (`template` the
    /// first expression, `args` the [`FmtArg`] rows of its `name = expr` arguments), so an
    /// argument keeps its words' hygiene and `$crate`; only the template's text is read.
    pub(crate) fn builtin_format_args(
        &mut self,
        template: Option<(ExprId, Range)>,
        args: &[(Node, Range)],
        r: Range,
    ) -> Result<ExprId, Why> {
        let Some((t, tr)) = template else { return Err(Why::NonStringTemplate) };
        if !matches!(Expr::decode(self.out.exprs[t.0 as usize]), Ok(Expr::Lit(_))) {
            return Err(Why::NonStringTemplate);
        }
        let Some((cs, ce)) = string_contents(tr.of(self.text())) else { return Err(Why::NonStringTemplate) };
        let template = Range::new(tr.start + idx(cs), tr.start + idx(ce));
        let alloc = *self.vals.allocator();
        let mut written: AVec<'_, Written> = Vec::new_in(alloc);
        for &(node, range) in args {
            let a = FmtArg::decode(node).map_err(|_| Why::NonStringTemplate)?;
            written.push(Written { name: a.name, expr: a.expr, range });
        }
        let raw = tr.of(self.text()).starts_with('r');
        self.format_args_of(template, raw, &written, false, r)
    }

    /// Lowers `format_args!(input)` (`input` the token tree's range, delimiters included).
    /// `Err` names why it is no builtin form: its template is not a string literal.
    pub(crate) fn format_args(&mut self, newline: bool, input: Range, r: Range) -> Result<ExprId, Why> {
        let pe = crate::parser_edition(self.edition);
        let inner = Range::new(input.start + 1, input.end.saturating_sub(1).max(input.start + 1));
        // The input's text, copied out of the machine's (which an expansion lowered here may
        // grow), and read by ranges of the machine's.
        let alloc = *self.vals.allocator();
        let mut owned = blaze_own::AString::new_in(alloc);
        owned.push_str(inner.of(self.text()));
        let of = |r: Range| &owned[(r.start - inner.start) as usize..(r.end - inner.start) as usize];
        // The template: the first token, a string, then `,` or the end.
        let lexed = LexedStr::new(pe, of(inner));
        let Some(first) = non_trivia(&lexed, 0) else { return Err(Why::NonStringTemplate) };
        if lexed.kind(first) != SyntaxKind::STRING {
            return Err(Why::NonStringTemplate);
        }
        let tok_start = inner.start + idx(lexed.text_start(first));
        let tok_text = lexed.text(first);
        let raw = tok_text.starts_with('r');
        let Some((cs, ce)) = string_contents(tok_text) else { return Err(Why::NonStringTemplate) };
        let template = Range::new(tok_start + idx(cs), tok_start + idx(ce));
        let mut at = inner.start + idx(lexed.text_start(first) + lexed.text_len(first));
        // The written arguments, each `, [name =] expr`.
        let mut written: AVec<'_, Written> = Vec::new_in(alloc);
        loop {
            let rest = Range::new(at, inner.end);
            let lexed = LexedStr::new(pe, of(rest));
            let Some(comma) = non_trivia(&lexed, 0) else { break };
            if lexed.kind(comma) != SyntaxKind::COMMA {
                self.diag(DiagKind::Syntax, Range::new(rest.start + idx(lexed.text_start(comma)), rest.end));
                break;
            }
            let Some(head) = non_trivia(&lexed, comma + 1) else { break };
            let mut name = None;
            let mut expr_at = head;
            if lexed.kind(head) == SyntaxKind::IDENT
                && let Some(eq) = non_trivia(&lexed, head + 1)
                && lexed.kind(eq) == SyntaxKind::EQ
                && (eq + 1 >= lexed.len() || lexed.kind(eq + 1) != SyntaxKind::EQ)
                && let Some(e) = non_trivia(&lexed, eq + 1)
            {
                let nr = Range::new(rest.start + idx(lexed.text_start(head)), rest.start + idx(lexed.text_start(head) + lexed.text_len(head)));
                name = Some(self.name(nr));
                expr_at = e;
            }
            let expr_start = rest.start + idx(lexed.text_start(expr_at));
            let expr_text = Range::new(expr_start, inner.end);
            let sub = LexedStr::new(pe, of(expr_text));
            let sub_input = sub.to_input(pe);
            let output = PrefixEntryPoint::Expr.parse(sub_input.tokens());
            let base = self.open_container();
            crate::feed::feed(self, &sub, &output, expr_start);
            let expr = self.vals[base..].iter().rev().find(|v| v.v == V::Expr).map(|v| (ExprId(v.id), v.range));
            self.close_container(base);
            // Where the expression ended: the token after the last it took.
            let taken = consumed(&output);
            let mut seen = 0usize;
            let mut end = sub.len();
            for i in 0..sub.len() {
                if seen == taken {
                    end = i;
                    break;
                }
                if !sub.kind(i).is_trivia() {
                    seen += 1;
                }
            }
            let end_at = expr_start + idx(if end < sub.len() { sub.text_start(end) } else { sub.as_str().len() });
            let Some((expr, range)) = expr else {
                self.diag(DiagKind::Syntax, Range::new(expr_start, inner.end));
                break;
            };
            written.push(Written { name, expr, range });
            if end_at <= at {
                break;
            }
            at = end_at;
        }
        self.format_args_of(template, raw, &written, newline, r)
    }

    /// The node of `format_args!` with `template` (its string's contents) and the `written`
    /// arguments: the template's pieces, and the arguments it captures.
    ///
    /// The template is parsed from its VALUE ([`template_value`]), as rustc parses it: the
    /// `{` of an escape `\u{e9}` is no brace, and `\x7b` is one. The pieces keep the record's
    /// ranges in the template's SOURCE (front2 fmt_template.rs unescapes them), through the
    /// value's map back to the source.
    fn format_args_of(&mut self, template: Range, raw: bool, written: &[Written], newline: bool, r: Range) -> Result<ExprId, Why> {
        let alloc = *self.vals.allocator();
        let tv = template_value(template.of(self.text()), raw, alloc);
        let base = template.start;
        if let Some(bad) = &tv.bad {
            // The literal's rules refuse the escape (the Reference, "String literals"); its
            // value is not a template to cut.
            self.diag(DiagKind::Syntax, Range::new(base + idx(bad.start), base + idx(bad.end)));
        }
        // The source position of value byte `k`.
        let at = |k: usize| base + tv.src[k];
        let text = tv.value.as_str();
        let bytes = text.as_bytes();
        // The template's pieces, and the arguments it captures.
        let mut pieces: AVec<'_, (Node, Range)> = Vec::new_in(alloc);
        let mut captured: AVec<'_, (u64, NameIdx, ExprId, Range)> = Vec::new_in(alloc);
        let mut next = 0u32;
        let mut i = 0usize;
        let mut run = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'{' if bytes.get(i + 1) == Some(&b'{') => i += 2,
                b'}' if bytes.get(i + 1) == Some(&b'}') => i += 2,
                b'{' => {
                    if run < i {
                        let piece = BodyRange::building(Range::new(at(run), at(i)));
                        pieces.push((FmtPiece::Text(piece).encode(), template));
                    }
                    let Some(close) = bytes[i + 1..].iter().position(|b| *b == b'}').map(|p| i + 1 + p) else {
                        self.diag(DiagKind::Syntax, Range::new(at(i), template.end));
                        run = bytes.len();
                        break;
                    };
                    let inside = &text[i + 1..close];
                    let colon = inside.find(':');
                    // The spec by value offsets, and by its source range for the record.
                    let (arg, spec_v) = match colon {
                        Some(c) => (&inside[..c], i + 2 + c..close),
                        None => (inside, close..close),
                    };
                    let spec = Range::new(at(spec_v.start), at(spec_v.end));
                    let spec_text = &text[spec_v.clone()];
                    // `name$` and `n$` in the spec name arguments; `.*` takes a positional
                    // before the value's own.
                    for (d, _) in spec_text.match_indices('$') {
                        let before = &spec_text[..d];
                        let start = before
                            .char_indices()
                            .rev()
                            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                            .last()
                            .map_or(d, |(s, _)| s);
                        let word = &before[start..];
                        if word.chars().next().is_some_and(|c| !c.is_ascii_digit()) {
                            let (ws, we) = (spec_v.start + start, spec_v.start + d);
                            self.fmt_named(&written, &mut captured, &text[ws..we], Range::new(at(ws), at(we)));
                        }
                    }
                    if spec_text.contains(".*") {
                        next += 1;
                    }
                    let arg_text = arg.trim();
                    let positional = |index: u32| (index as usize) < written.len();
                    let index = if arg_text.is_empty() {
                        next += 1;
                        if !positional(next - 1) {
                            self.diag(DiagKind::FormatArgMissing, Range::new(at(i), at(close + 1)));
                        }
                        next - 1
                    } else if let Ok(n) = arg_text.parse::<u32>() {
                        if !positional(n) {
                            self.diag(DiagKind::FormatArgMissing, Range::new(at(i), at(close + 1)));
                        }
                        n
                    } else {
                        let ws = i + 1 + (arg.len() - arg.trim_start().len());
                        let we = ws + arg_text.len();
                        self.fmt_named(&written, &mut captured, &text[ws..we], Range::new(at(ws), at(we)))
                    };
                    let piece = FmtPiece::Arg { arg: index, spec: BodyRange::building(spec) };
                    pieces.push((piece.encode(), Range::new(at(i), at(close + 1))));
                    i = close + 1;
                    run = i;
                }
                b'}' => {
                    // rustc: "invalid format string: unmatched `}` found".
                    self.diag(DiagKind::Syntax, Range::new(at(i), at(i + 1)));
                    i += 1;
                }
                _ => i += 1,
            }
        }
        if run < bytes.len() {
            let piece = BodyRange::building(Range::new(at(run), template.end));
            pieces.push((FmtPiece::Text(piece).encode(), template));
        }
        let pieces = self.put_rows_of(&pieces);
        let mut args: AVec<'_, (Node, Range)> = Vec::new_in(alloc);
        for w in written.iter() {
            args.push((FmtArg { name: w.name, expr: w.expr, captured: false }.encode(), w.range));
        }
        for &(_, name, expr, range) in captured.iter() {
            args.push((FmtArg { name: Some(name), expr, captured: true }.encode(), range));
        }
        let args = self.put_rows_of(&args);
        let row = self.put_row(FormatArgs { template: BodyRange::building(template), pieces, args, newline }.encode(), r);
        Ok(self.put_expr(Expr::FormatArgs { row }, r))
    }

    /// The argument `{name}` names at `wr`: a written one by its name, else one captured (made
    /// once per name). Its position among all the arguments.
    ///
    /// `text` is the name in the template's VALUE, and `wr` its source range: they differ only
    /// where an escape spells the name (`{\x61}` is `{a}`).
    fn fmt_named(
        &mut self,
        written: &[Written],
        captured: &mut AVec<'_, (u64, NameIdx, ExprId, Range)>,
        text: &str,
        wr: Range,
    ) -> u32 {
        let spelled = text.strip_prefix("r#").unwrap_or(text);
        let hash = blaze_typeck::Name::of(spelled).0;
        if let Some(at) = written.iter().position(|w| w.name.is_some_and(|n| self.out.names[n.0 as usize] == hash)) {
            return idx(at);
        }
        if let Some(at) = captured.iter().position(|c| c.0 == hash) {
            return idx(written.len() + at);
        }
        let name = if wr.of(self.text()) == text { self.name(wr) } else { self.synth_name(spelled, wr) };
        let seg = Segment {
            kind: SegKind::Ident,
            name: Some(name),
            args: List { start: 0, len: 0 },
            has_args: false,
            parenthesized: false,
            return_notation: false,
            ty: None,
            trait_: None,
        };
        let seg = self.put_row(seg.encode(), wr);
        let path = PathRef { segments: List { start: seg.0, len: 1 }, absolute: false, scope: Some(self.scope()), lang: None };
        let path = self.put_path(path, wr);
        let e = self.path_expr(path);
        let e = self.put_expr(e, wr);
        captured.push((hash, name, e, wr));
        idx(written.len() + captured.len() - 1)
    }
}
