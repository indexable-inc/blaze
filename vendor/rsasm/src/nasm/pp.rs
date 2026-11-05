//! The NASM preprocessor, as NASM 2.16.03 behaves.
//!
//! A line is handled in NASM's order: inside a multi-line macro its `%1`,
//! `%0`, `%%name` and `%?` are replaced first; then a `%` directive is
//! carried out, or the line skipped if a `%if` is false; then single-line
//! macros are expanded; then, if the line calls a multi-line macro, the call
//! is expanded, and otherwise the line is assembled.

use super::token::{self, Kind, Tok};
use super::{Cond, Context, Frame, MMacro, SMacro, Unwind};
use crate::assembler::{Assembler, Reader};
use crate::cursor::Cursor;
use crate::lexer::{Lexer, TokKind, Token};
use crate::parser::Parser;
use crate::source::Span;
use std::rc::Rc;

/// How deeply expansions may nest, macro calls, `%rep` blocks and single-line
/// macros alike, before rsasm calls it runaway recursion.
const MAX_DEPTH: u32 = 64;

/// NASM's default limit on a `%rep` count (`--limit-rep`).
const MAX_REP: i64 = 1_000_000;

/// One logical line: physical lines joined where one ends in a backslash.
pub(crate) struct RawLine {
    /// Offset in the file where the line starts.
    pub start: usize,
    /// Offset just past its end of line.
    pub next: usize,
    pub text: String,
    pub span: Span,
}

/// The condition codes `%+1` and `%-1` accept, with their inverses.
const CONDITIONS: &[(&str, &str)] = &[
    ("a", "be"),
    ("ae", "b"),
    ("b", "ae"),
    ("be", "a"),
    ("c", "nc"),
    ("e", "ne"),
    ("g", "le"),
    ("ge", "l"),
    ("l", "ge"),
    ("le", "g"),
    ("na", "a"),
    ("nae", "ae"),
    ("nb", "b"),
    ("nbe", "be"),
    ("nc", "c"),
    ("ne", "e"),
    ("ng", "g"),
    ("nge", "ge"),
    ("nl", "l"),
    ("nle", "le"),
    ("no", "o"),
    ("np", "p"),
    ("ns", "s"),
    ("nz", "z"),
    ("o", "no"),
    ("p", "np"),
    ("pe", "po"),
    ("po", "pe"),
    ("s", "ns"),
    ("z", "nz"),
];

impl Assembler {
    /// Reads NASM standard macros, before any source.
    pub(crate) fn nasm_prelude(&mut self) {
        let mut text = super::stdmac::COMMON.to_string();
        if !self.options.relocatable {
            text.push_str(super::stdmac::BIN);
        }
        let file = self.sm.add("<nasm standard macros>", text);
        self.assemble_file(file);
    }

    /// Walks a file's lines through the preprocessor.
    pub(crate) fn run_nasm(&mut self, reader: &mut Reader) {
        let conds = self.nasm.conds.len();
        let open_span = reader_span(self, reader);
        while let Some(line) = self.nasm_read_line(reader) {
            if self.diags.saturated() {
                return;
            }
            if !self.nasm_line(line, reader) {
                break;
            }
        }
        if self.nasm.conds.len() > conds {
            if self.nasm.unwind.is_none() && !self.end_of_source {
                self.diags
                    .error(open_span, "unterminated `%if`, expected `%endif`");
            }
            self.nasm.conds.truncate(conds);
        }
    }

    /// The next logical line of `reader`'s file, without moving past it.
    pub(crate) fn nasm_read_line(&self, reader: &Reader) -> Option<RawLine> {
        let file = self.sm.file(reader.parser.file());
        let src = file.src.as_str();
        let start = reader.parser.offset();
        if start >= src.len() {
            return None;
        }
        let mut pos = start;
        let mut text = String::new();
        loop {
            let end = src[pos..].find('\n').map_or(src.len(), |i| pos + i);
            let line = &src[pos..end];
            let line = line.strip_suffix('\r').unwrap_or(line);
            if end < src.len()
                && let Some(joined) = line.strip_suffix('\\')
            {
                text.push_str(joined);
                pos = end + 1;
                continue;
            }
            text.push_str(line);
            let next = if end < src.len() { end + 1 } else { end };
            return Some(RawLine {
                start,
                next,
                text,
                span: Span::new(file.start + start as u32, file.start + end as u32),
            });
        }
    }

    /// Handles one line. Returns whether the walk goes on.
    fn nasm_line(&mut self, line: RawLine, reader: &mut Reader) -> bool {
        let mut toks = token::tokenize(&line.text);
        let mut changed = false;
        if let Some(t) = self.nasm_expand_params(&toks, line.span) {
            // Pasting happens as the parameters go in: `foo%1` is one name.
            toks = token::tokenize(&token::render(&t));
            changed = true;
        }
        let first = toks.iter().position(|t| !t.is_space());
        if let Some(i) = first
            && toks[i].kind == Kind::Pp
            && toks[i].text[1..].starts_with(|c: char| c.is_ascii_alphabetic())
        {
            reader.parser.set_offset(line.next);
            let name = toks[i].text[1..].to_ascii_lowercase();
            let args = &toks[i + 1..];
            return self.nasm_directive(&name, args, &line, reader);
        }
        if !self.nasm_emitting() || first.is_none() {
            reader.parser.set_offset(line.next);
            return true;
        }
        if let Some(t) = self.nasm_expand_smacros(&toks, line.span) {
            toks = t;
            changed = true;
        }
        if let Some(call) = self.nasm_find_call(&toks, line.span) {
            reader.parser.set_offset(line.next);
            self.nasm_call(call, line.span);
            return self.nasm_goes_on();
        }
        if changed {
            reader.parser.set_offset(line.next);
            let text = token::render(&toks);
            self.nasm_assemble_text(&text, line.span);
        } else {
            match self.next_statement(reader) {
                Some(stmt) => {
                    self.nasm_statement(&stmt);
                    reader.parser.recycle(stmt);
                }
                None => reader.parser.set_offset(line.next),
            }
            if reader.parser.offset() < line.next {
                reader.parser.set_offset(line.next);
            }
        }
        self.nasm_goes_on()
    }

    fn nasm_goes_on(&self) -> bool {
        self.nasm.unwind.is_none() && !self.end_of_source && !self.diags.saturated()
    }

    fn nasm_emitting(&self) -> bool {
        self.nasm.conds.last().is_none_or(|c| c.emitting())
    }

    /// Assembles a preprocessed line of text, which goes through the
    /// preprocessor no further.
    pub(crate) fn nasm_assemble_text(&mut self, text: &str, origin: Span) {
        let name = self.nasm_origin_name(origin);
        let file = self.sm.add(name, text.to_string());
        let mut parser = Parser::new(file, self.lex_config());
        while let Some(stmt) = parser.next_statement(
            &self.sm,
            &mut self.interner,
            &mut self.pool,
            &mut self.diags,
        ) {
            self.nasm_statement(&stmt);
            parser.recycle(stmt);
        }
    }

    /// A name for text expanded from the line at `origin`.
    fn nasm_origin_name(&self, origin: Span) -> String {
        match self.sm.lookup(origin.lo) {
            Some(f) => format!(
                "{}:{} (expanded)",
                f.name.display(),
                f.line_col(origin.lo).line
            ),
            None => "<expansion>".to_string(),
        }
    }

    // ---- directives ---------------------------------------------------------

    fn nasm_directive(
        &mut self,
        name: &str,
        args: &[Tok],
        line: &RawLine,
        reader: &mut Reader,
    ) -> bool {
        let span = line.span;
        if let Some(cond) = parse_condition(name) {
            self.nasm_conditional(cond, args, span);
            return true;
        }
        if !self.nasm_emitting() {
            return true;
        }
        let args = token::trim(args);
        match name {
            "define" | "idefine" | "xdefine" | "ixdefine" => {
                let insensitive = name.starts_with('i');
                let expand = name.ends_with("xdefine");
                self.nasm_define(args, insensitive, expand, span);
            }
            "undef" | "undefalias" => {
                if let Some((n, _)) = self.nasm_macro_name(args, span) {
                    self.nasm_undef(&n);
                }
            }
            "defalias" | "idefalias" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let target = token::render(token::trim(&rest));
                    self.nasm.aliases.insert(n, target);
                }
            }
            "assign" | "iassign" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span)
                    && let Some(v) = self.nasm_eval(&rest, span, "`%assign`")
                {
                    let body = number_tokens(v);
                    self.nasm_store_smacro(&n, name == "iassign", None, false, body);
                }
            }
            "defstr" | "idefstr" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    let text: String = rest
                        .iter()
                        .filter(|t| !t.is_space())
                        .map(|t| t.text.as_str())
                        .collect();
                    let body = vec![quote(text.as_bytes())];
                    self.nasm_store_smacro(&n, name == "idefstr", None, false, body);
                }
            }
            "deftok" | "ideftok" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    let rest = token::trim(&rest);
                    match rest.first().and_then(Tok::string_value) {
                        Some(s) if rest.len() == 1 => {
                            let body = token::tokenize(&String::from_utf8_lossy(&s));
                            self.nasm_store_smacro(&n, name == "ideftok", None, false, body);
                        }
                        _ => self.diags.error(span, "`%deftok` requires a string"),
                    }
                }
            }
            "strlen" | "istrlen" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    let rest = token::trim(&rest);
                    match rest.first().and_then(Tok::string_value) {
                        Some(s) if rest.len() == 1 => {
                            let body = number_tokens(s.len() as i64);
                            self.nasm_store_smacro(&n, name == "istrlen", None, false, body);
                        }
                        _ => self.diags.error(span, "`%strlen` requires a string"),
                    }
                }
            }
            "strcat" | "istrcat" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    let mut bytes = Vec::new();
                    for t in rest.iter().filter(|t| !t.is_space() && !t.is(",")) {
                        match t.string_value() {
                            Some(s) => bytes.extend(s),
                            None => {
                                self.diags.error(
                                    span,
                                    format!("`%strcat` takes only strings, not `{}`", t.text),
                                );
                                return true;
                            }
                        }
                    }
                    let body = vec![quote(&bytes)];
                    self.nasm_store_smacro(&n, name == "istrcat", None, false, body);
                }
            }
            "substr" | "isubstr" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    if let Some(body) = self.nasm_substr(&rest, span) {
                        self.nasm_store_smacro(&n, name == "isubstr", None, false, body);
                    }
                }
            }
            "pathsearch" | "ipathsearch" => {
                if let Some((n, rest)) = self.nasm_macro_name(args, span) {
                    let rest = self.nasm_expand_or_keep(&rest, span);
                    let rest = token::trim(&rest);
                    let file = match rest.first().and_then(Tok::string_value) {
                        Some(s) => String::from_utf8_lossy(&s).into_owned(),
                        None => token::render(rest),
                    };
                    let found = self
                        .find_include(&file)
                        .map_or(file, |p| p.display().to_string());
                    let body = vec![quote(found.as_bytes())];
                    self.nasm_store_smacro(&n, name == "ipathsearch", None, false, body);
                }
            }
            "depend" | "use" | "line" | "pragma" | "clear" | "aliases" | "require" | "note" => {}
            "macro" | "imacro" | "rmacro" | "irmacro" => {
                self.nasm_define_mmacro(name, args, span, reader);
            }
            "endmacro" | "endm" => {
                self.diags
                    .error(span, "`%endmacro` without a matching `%macro`");
            }
            "exitmacro" => {
                if self.nasm.frames.is_empty() {
                    self.diags.error(span, "`%exitmacro` outside a macro call");
                } else {
                    self.nasm.unwind = Some(Unwind::Macro);
                    return false;
                }
            }
            "unmacro" | "unimacro" => self.nasm_unmacro(name == "unimacro", args, span),
            "rotate" => self.nasm_rotate(args, span),
            "rep" => {
                self.nasm_rep(args, span, reader);
                return self.nasm_goes_on();
            }
            "endrep" => self
                .diags
                .error(span, "`%endrep` without a matching `%rep`"),
            "exitrep" => {
                let floor = self.nasm.frames.last().map_or(0, |f| f.reps);
                if self.nasm.reps <= floor {
                    self.diags.error(span, "`%exitrep` outside a `%rep` block");
                } else {
                    self.nasm.unwind = Some(Unwind::Rep);
                    return false;
                }
            }
            "include" => {
                let rest = self.nasm_expand_or_keep(args, span);
                let rest = token::trim(&rest);
                let file = match rest.first().and_then(Tok::string_value) {
                    Some(s) => String::from_utf8_lossy(&s).into_owned(),
                    None => token::render(rest),
                };
                match self.find_include(&file) {
                    Some(path) => {
                        self.include(&path, span);
                        return self.nasm_goes_on();
                    }
                    None => self
                        .diags
                        .error(span, format!("unable to open include file `{file}`")),
                }
            }
            "push" => {
                let name = token::render(args);
                self.nasm.next_context += 1;
                let id = self.nasm.next_context;
                self.nasm.contexts.push(Context {
                    name,
                    id,
                    smacros: Default::default(),
                });
            }
            "pop" => {
                if self.nasm.contexts.pop().is_none() {
                    self.diags.error(span, "`%pop`: context stack is empty");
                }
            }
            "repl" => match self.nasm.contexts.last_mut() {
                Some(c) => c.name = token::render(args),
                None => self.diags.error(span, "`%repl`: context stack is empty"),
            },
            "error" | "fatal" | "warning" => {
                let rest = self.nasm_expand_or_keep(args, span);
                let rest = token::trim(&rest);
                let msg = match rest {
                    [t] if t.kind == Kind::Str => {
                        String::from_utf8_lossy(&t.string_value().unwrap_or_default()).into_owned()
                    }
                    _ => token::render(rest),
                };
                if name == "warning" {
                    self.diags.warning(span, msg);
                } else {
                    self.diags.error(span, msg);
                    if name == "fatal" {
                        self.end_of_source = true;
                        return false;
                    }
                }
            }
            "arg" | "local" | "stacksize" => {
                self.diags.error(
                    span,
                    format!("`%{name}` is not supported; write the stack offsets out"),
                );
            }
            _ => self
                .diags
                .error(span, format!("unknown preprocessor directive `%{name}`")),
        }
        true
    }

    /// The macro name at the start of a directive's arguments, and what
    /// follows it. A context-local `%$name` keeps its `%$` prefix.
    fn nasm_macro_name(&mut self, args: &[Tok], span: Span) -> Option<(String, Vec<Tok>)> {
        let args = token::trim(args);
        match args.first() {
            Some(t)
                if t.kind == Kind::Ident || (t.kind == Kind::Pp && t.text.starts_with("%$")) =>
            {
                Some((t.text.clone(), args[1..].to_vec()))
            }
            _ => {
                self.diags.error(span, "expected a macro name");
                None
            }
        }
    }

    fn nasm_expand_or_keep(&mut self, toks: &[Tok], span: Span) -> Vec<Tok> {
        self.nasm_expand_smacros(toks, span)
            .unwrap_or_else(|| toks.to_vec())
    }

    fn nasm_define(&mut self, args: &[Tok], insensitive: bool, expand: bool, span: Span) {
        let Some((name, rest)) = self.nasm_macro_name(args, span) else {
            return;
        };
        // Parameters only when the parenthesis follows the name directly.
        let mut params = None;
        let mut greedy = false;
        let mut body = rest.as_slice();
        if body.first().is_some_and(|t| t.is("(")) {
            let Some(close) = body.iter().position(|t| t.is(")")) else {
                self.diags
                    .error(span, "expected `)` to close the parameter list");
                return;
            };
            let mut names = Vec::new();
            for piece in token::split_args(&body[1..close]) {
                match piece.as_slice() {
                    [] => {}
                    [t] if t.kind == Kind::Ident => names.push(t.text.clone()),
                    [t, plus] if t.kind == Kind::Ident && plus.is("+") => {
                        names.push(t.text.clone());
                        greedy = true;
                    }
                    _ => {
                        self.diags.error(
                            span,
                            format!("`{}` is not a parameter name", token::render(&piece)),
                        );
                        return;
                    }
                }
            }
            params = Some(names);
            body = &body[close + 1..];
        }
        let mut body = token::trim(body).to_vec();
        if expand {
            body = self.nasm_expand_or_keep(&body, span);
        }
        self.nasm_store_smacro(&name, insensitive, params, greedy, body);
    }

    /// Defines a single-line macro, replacing any with the same number of
    /// parameters.
    fn nasm_store_smacro(
        &mut self,
        name: &str,
        insensitive: bool,
        params: Option<Vec<String>>,
        greedy: bool,
        body: Vec<Tok>,
    ) {
        let arity = params.as_ref().map(Vec::len);
        let mac = SMacro {
            params,
            greedy,
            body,
        };
        let table = if let Some(local) = name.strip_prefix('%') {
            let depth = local.chars().take_while(|&c| c == '$').count();
            let key = local[depth..].to_string();
            let n = self.nasm.contexts.len();
            if depth > n {
                self.diags
                    .error(Span::DUMMY, format!("no context for `{name}`"));
                return;
            }
            let ctx = &mut self.nasm.contexts[n - depth];
            ctx.smacros.entry(key).or_default()
        } else if insensitive {
            self.nasm
                .ismacros
                .entry(name.to_ascii_lowercase())
                .or_default()
        } else {
            self.nasm.smacros.entry(name.to_string()).or_default()
        };
        table.retain(|m| m.params.as_ref().map(Vec::len) != arity);
        table.push(mac);
    }

    fn nasm_undef(&mut self, name: &str) {
        if let Some(local) = name.strip_prefix('%') {
            let depth = local.chars().take_while(|&c| c == '$').count();
            let n = self.nasm.contexts.len();
            if depth <= n {
                self.nasm.contexts[n - depth]
                    .smacros
                    .remove(&local[depth..]);
            }
            return;
        }
        self.nasm.smacros.remove(name);
        self.nasm.ismacros.remove(&name.to_ascii_lowercase());
        self.nasm.aliases.remove(name);
    }

    fn nasm_substr(&mut self, rest: &[Tok], span: Span) -> Option<Vec<Tok>> {
        let rest = token::trim(rest);
        let Some(s) = rest.first().and_then(Tok::string_value) else {
            self.diags.error(span, "`%substr` requires a string");
            return None;
        };
        let mut args = &rest[1..];
        args = token::trim(args);
        if args.first().is_some_and(|t| t.is(",")) {
            args = &args[1..];
        }
        let pieces = token::split_args(args);
        let start = self.nasm_eval(pieces.first()?, span, "`%substr` start")?;
        let count = match pieces.get(1) {
            Some(p) => self.nasm_eval(p, span, "`%substr` length")?,
            None => 1,
        };
        let len = s.len() as i64;
        let mut start = start - 1;
        let mut count = count;
        if count < 0 {
            count = len + count + 1 - start;
        }
        if start < 0 {
            count += start;
            start = 0;
        }
        let start = start.min(len);
        let count = count.clamp(0, len - start);
        Some(vec![quote(&s[start as usize..(start + count) as usize])])
    }

    // ---- conditionals ---------------------------------------------------

    fn nasm_conditional(&mut self, cond: Condition, args: &[Tok], span: Span) {
        match cond.op {
            CondOp::If => {
                let state = if self.nasm_emitting() {
                    if self.nasm_test(cond, args, span) {
                        Cond::IfTrue
                    } else {
                        Cond::IfFalse
                    }
                } else {
                    Cond::Never
                };
                self.nasm.conds.push(state);
            }
            CondOp::Elif => {
                let Some(&top) = self.nasm.conds.last() else {
                    self.diags.error(span, "`%elif` without a matching `%if`");
                    return;
                };
                let next = match top {
                    Cond::IfTrue | Cond::Done => Cond::Done,
                    Cond::IfFalse => {
                        if self.nasm_test(cond, args, span) {
                            Cond::IfTrue
                        } else {
                            Cond::IfFalse
                        }
                    }
                    Cond::Never => Cond::Never,
                    Cond::ElseTrue | Cond::ElseFalse => {
                        self.diags.error(span, "`%elif` after `%else`");
                        Cond::Never
                    }
                };
                *self.nasm.conds.last_mut().expect("checked") = next;
            }
            CondOp::Else => {
                let Some(top) = self.nasm.conds.last_mut() else {
                    self.diags.error(span, "`%else` without a matching `%if`");
                    return;
                };
                *top = match *top {
                    Cond::IfTrue | Cond::Done => Cond::ElseFalse,
                    Cond::IfFalse => Cond::ElseTrue,
                    Cond::Never => Cond::Never,
                    Cond::ElseTrue | Cond::ElseFalse => {
                        self.diags.error(span, "`%else` after `%else`");
                        Cond::Never
                    }
                };
            }
            CondOp::Endif => {
                if self.nasm.conds.pop().is_none() {
                    self.diags.error(span, "`%endif` without a matching `%if`");
                }
            }
        }
    }

    fn nasm_test(&mut self, cond: Condition, args: &[Tok], span: Span) -> bool {
        let value = match cond.kind {
            CondKind::Expr => self
                .nasm_eval(args, span, "`%if` condition")
                .is_some_and(|v| v != 0),
            CondKind::Def => {
                let args = token::trim(args);
                args.iter()
                    .filter(|t| !t.is_space())
                    .all(|t| self.nasm_smacro_defined(&t.text))
                    && !args.is_empty()
            }
            CondKind::Macro => {
                let args = token::trim(args);
                let Some(first) = args.first() else {
                    return false;
                };
                let count = token::render(&args[1..]);
                let count = count.trim();
                let lower = first.text.to_ascii_lowercase();
                let candidates = self
                    .nasm
                    .mmacros
                    .get(&first.text)
                    .into_iter()
                    .flatten()
                    .chain(self.nasm.immacros.get(&lower).into_iter().flatten());
                let mut any = false;
                for m in candidates {
                    any |= match count.parse::<usize>() {
                        Ok(n) => n >= m.min && (m.greedy || n <= m.max),
                        Err(_) => true,
                    };
                }
                any
            }
            CondKind::Idn | CondKind::Idni => {
                let expanded = self.nasm_expand_or_keep(args, span);
                let Some(comma) = expanded.iter().position(|t| t.is(",")) else {
                    self.diags
                        .error(span, "`%ifidn` expects two comma-separated arguments");
                    return false;
                };
                let a = significant(&expanded[..comma]);
                let b = significant(&expanded[comma + 1..]);
                let same = |x: &Tok, y: &Tok| match (x.string_value(), y.string_value()) {
                    (Some(p), Some(q)) => p == q,
                    _ if cond.kind == CondKind::Idni => x.text.eq_ignore_ascii_case(&y.text),
                    _ => x.text == y.text,
                };
                a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| same(x, y))
            }
            CondKind::Num | CondKind::Str | CondKind::Id | CondKind::Token | CondKind::Empty => {
                let expanded = self.nasm_expand_or_keep(args, span);
                let toks = significant(&expanded);
                match cond.kind {
                    CondKind::Empty => toks.is_empty(),
                    CondKind::Token => toks.len() == 1,
                    _ => {
                        toks.len() == 1
                            && toks[0].kind
                                == match cond.kind {
                                    CondKind::Num => Kind::Number,
                                    CondKind::Str => Kind::Str,
                                    _ => Kind::Ident,
                                }
                    }
                }
            }
            CondKind::Ctx => {
                let names = significant(args);
                match self.nasm.contexts.last() {
                    Some(ctx) => names.iter().any(|t| t.text.eq_ignore_ascii_case(&ctx.name)),
                    None => false,
                }
            }
            CondKind::Env => {
                let names = significant(args);
                names.iter().any(|t| {
                    let n = t
                        .string_value()
                        .map_or(t.text.clone(), |s| String::from_utf8_lossy(&s).into_owned());
                    std::env::var_os(n).is_some()
                })
            }
        };
        value != cond.negate
    }

    fn nasm_smacro_defined(&self, name: &str) -> bool {
        if let Some(local) = name.strip_prefix('%') {
            let depth = local.chars().take_while(|&c| c == '$').count();
            let n = self.nasm.contexts.len();
            return depth > 0
                && depth <= n
                && self.nasm.contexts[n - depth]
                    .smacros
                    .contains_key(&local[depth..]);
        }
        self.nasm.smacros.contains_key(name)
            || self.nasm.ismacros.contains_key(&name.to_ascii_lowercase())
            || self.nasm.aliases.contains_key(name)
            || builtin_name(name)
    }

    /// Evaluates tokens as a critical expression: macros expanded, and every
    /// symbol in it already defined.
    pub(crate) fn nasm_eval(&mut self, toks: &[Tok], span: Span, what: &str) -> Option<i64> {
        let expanded = self.nasm_expand_or_keep(toks, span);
        let text = token::render(&expanded);
        let toks = self.nasm_lex(&text, span);
        let mut cur = Cursor::new(&toks);
        if cur.at_end() {
            self.diags
                .error(span, format!("{what} needs an expression"));
            return None;
        }
        let toks = self.nasm_rewrite_locals(&toks);
        let mut cur2 = Cursor::new(&toks);
        let mark = self.exprs.len();
        let e = self.parse_expr(&mut cur2)?;
        cur.set_pos(cur2.pos());
        self.expect_end(&mut cur2);
        self.nasm_bind_positional(mark, None);
        let v = self.eval(e).ok()?;
        match v.as_abs().or_else(|| self.nasm_fixed_value(v)) {
            Some(n) => Some(n),
            None => {
                self.diags.error(span, format!("{what} must be a constant"));
                None
            }
        }
    }

    /// Lexes text by the NASM rules, as a file of its own named after
    /// `origin`, and returns its tokens up to the first end of line.
    pub(crate) fn nasm_lex(&mut self, text: &str, origin: Span) -> Vec<Token> {
        let name = self.nasm_origin_name(origin);
        let file = self.sm.add(name, text.to_string());
        let config = self.lex_config();
        let Assembler {
            sm,
            interner,
            pool,
            diags,
            ..
        } = self;
        let mut lexer = Lexer::new(sm, file, config);
        let mut out = Vec::new();
        loop {
            let t = lexer.next_token(interner, pool, diags);
            match t.kind {
                TokKind::Eof => break,
                TokKind::Eol if out.is_empty() => {}
                TokKind::Eol => break,
                _ => out.push(t),
            }
        }
        out
    }

    // ---- single-line macros ---------------------------------------------

    /// Expands single-line macros and context-local names in a line, or
    /// returns `None` if there are none.
    pub(crate) fn nasm_expand_smacros(&mut self, toks: &[Tok], span: Span) -> Option<Vec<Tok>> {
        if !toks
            .iter()
            .any(|t| matches!(t.kind, Kind::Ident | Kind::Pp))
        {
            return None;
        }
        let mut changed = false;
        let mut active = Vec::new();
        let out = self.nasm_expand_in(toks, &mut active, &mut changed, span);
        let pastes = out.iter().any(|t| t.kind == Kind::Pp && t.text == "%+");
        (changed || pastes).then_some(out)
    }

    fn nasm_expand_in(
        &mut self,
        toks: &[Tok],
        active: &mut Vec<String>,
        changed: &mut bool,
        span: Span,
    ) -> Vec<Tok> {
        let mut out = Vec::with_capacity(toks.len());
        let mut i = 0;
        while i < toks.len() {
            let t = &toks[i];
            match t.kind {
                Kind::Ident => {
                    if active.len() < MAX_DEPTH as usize
                        && let Some((key, body, next)) =
                            self.nasm_smacro_call(toks, i, active, span)
                    {
                        *changed = true;
                        active.push(key);
                        let expanded = self.nasm_expand_in(&body, active, changed, span);
                        active.pop();
                        out.extend(expanded);
                        i = next;
                        continue;
                    }
                }
                Kind::Pp if t.text.starts_with("%$") || t.text.starts_with("%{$") => {
                    let name = t.text.trim_start_matches("%{").trim_start_matches('%');
                    let name = name.trim_end_matches('}');
                    *changed = true;
                    let depth = name.chars().take_while(|&c| c == '$').count();
                    let local = &name[depth..];
                    let n = self.nasm.contexts.len();
                    if depth > n {
                        self.diags.error(
                            span,
                            format!("`%{name}`: context stack has only {n} level(s)"),
                        );
                        i += 1;
                        continue;
                    }
                    let ctx = &self.nasm.contexts[n - depth];
                    let key = format!("%{name}");
                    let body = ctx
                        .smacros
                        .get(local)
                        .and_then(|v| v.iter().find(|m| m.params.is_none()))
                        .map(|m| m.body.clone());
                    match body {
                        Some(body) if !active.contains(&key) => {
                            active.push(key);
                            let expanded = self.nasm_expand_in(&body, active, changed, span);
                            active.pop();
                            out.extend(expanded);
                        }
                        _ => out.push(Tok::new(Kind::Ident, format!("..@{}.{local}", ctx.id))),
                    }
                    i += 1;
                    continue;
                }
                Kind::Pp if t.text == "%[" => {
                    let mut depth = 1;
                    let mut j = i + 1;
                    while j < toks.len() {
                        if toks[j].is("[") || (toks[j].kind == Kind::Pp && toks[j].text == "%[") {
                            depth += 1;
                        } else if toks[j].is("]") {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }
                    let inner =
                        self.nasm_expand_in(&toks[i + 1..j.min(toks.len())], active, changed, span);
                    let text: String = inner
                        .iter()
                        .filter(|t| !t.is_space())
                        .map(|t| t.text.as_str())
                        .collect();
                    out.extend(token::tokenize(&text));
                    *changed = true;
                    i = j + 1;
                    continue;
                }
                _ => {}
            }
            out.push(t.clone());
            i += 1;
        }
        out
    }

    /// The expansion of a single-line macro called at `toks[i]`, with the
    /// index just past the call.
    fn nasm_smacro_call(
        &mut self,
        toks: &[Tok],
        i: usize,
        active: &[String],
        span: Span,
    ) -> Option<(String, Vec<Tok>, usize)> {
        let mut name = toks[i].text.clone();
        for _ in 0..16 {
            match self.nasm.aliases.get(&name) {
                Some(target) => name = target.clone(),
                None => break,
            }
        }
        let (key, overloads) = if let Some(v) = self.nasm.smacros.get(&name) {
            (name.clone(), v.clone())
        } else if let Some(v) = self.nasm.ismacros.get(&name.to_ascii_lowercase()) {
            (name.to_ascii_lowercase(), v.clone())
        } else {
            let body = self.nasm_builtin(&name, span)?;
            return Some((name, body, i + 1));
        };
        if active.contains(&key) {
            return None;
        }
        // A call with arguments, if there is a parenthesis and a macro that
        // takes them.
        let mut j = i + 1;
        while toks.get(j).is_some_and(Tok::is_space) {
            j += 1;
        }
        if toks.get(j).is_some_and(|t| t.is("(")) && overloads.iter().any(|m| m.params.is_some()) {
            let mut depth = 0;
            let mut k = j;
            while k < toks.len() {
                if toks[k].is("(") {
                    depth += 1;
                } else if toks[k].is(")") {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                k += 1;
            }
            if k < toks.len() {
                let inner = &toks[j + 1..k];
                let args = split_call_args(inner);
                let n = if token::trim(inner).is_empty() {
                    0
                } else {
                    args.len()
                };
                let found = overloads.iter().find(|m| match &m.params {
                    Some(p) => p.len() == n || (m.greedy && n >= p.len()),
                    None => false,
                });
                if let Some(m) = found {
                    let params = m.params.as_ref().expect("has parameters");
                    let mut bound: Vec<Vec<Tok>> = args.into_iter().take(n).collect();
                    if m.greedy && n > params.len() {
                        let rest = bound.split_off(params.len() - 1);
                        let mut joined = Vec::new();
                        for (x, a) in rest.into_iter().enumerate() {
                            if x > 0 {
                                joined.push(Tok::new(Kind::Other, ","));
                            }
                            joined.extend(a);
                        }
                        bound.push(joined);
                    }
                    let body = m
                        .body
                        .iter()
                        .flat_map(|t| {
                            match params
                                .iter()
                                .position(|p| t.kind == Kind::Ident && *p == t.text)
                            {
                                Some(x) => bound.get(x).cloned().unwrap_or_default(),
                                None => vec![t.clone()],
                            }
                        })
                        .collect();
                    return Some((key, body, k + 1));
                }
            }
        }
        let m = overloads.iter().find(|m| m.params.is_none())?;
        Some((key, m.body.clone(), i + 1))
    }

    /// NASM's predefined single-line macros that change as source is read.
    fn nasm_builtin(&self, name: &str, span: Span) -> Option<Vec<Tok>> {
        let num = |v: i64| Some(number_tokens(v));
        match name {
            "__?LINE?__" => num(self
                .sm
                .lookup(span.lo)
                .map_or(0, |f| f.line_col(span.lo).line as i64)),
            "__?FILE?__" => {
                let f = self
                    .sm
                    .lookup(span.lo)
                    .map_or(String::new(), |f| f.name.display().to_string());
                Some(vec![quote(f.as_bytes())])
            }
            "__?BITS?__" => num(self.arch_state.bits as i64),
            "__?PTR?__" => Some(vec![Tok::new(
                Kind::Ident,
                match self.arch_state.bits {
                    64 => "qword",
                    32 => "dword",
                    _ => "word",
                },
            )]),
            "__?PASS?__" => num(2),
            "__?OUTPUT_FORMAT?__" => Some(vec![Tok::new(Kind::Ident, self.nasm_format())]),
            "__?NASM_MAJOR?__" => num(2),
            "__?NASM_MINOR?__" => num(16),
            "__?NASM_SUBMINOR?__" => num(3),
            "__?NASM_PATCHLEVEL?__" => num(0),
            "__?NASM_VERSION_ID?__" => num(0x0210_0300),
            "__?NASM_VER?__" => Some(vec![quote(b"2.16.03")]),
            _ => None,
        }
    }

    /// The name NASM's `-f` would give the output being written.
    pub(crate) fn nasm_format(&self) -> &'static str {
        if !self.options.relocatable {
            "bin"
        } else if crate::output::elf::is_elf64(self.target()) {
            "elf64"
        } else {
            "elf32"
        }
    }

    // ---- multi-line macros ------------------------------------------------

    fn nasm_define_mmacro(&mut self, dir: &str, args: &[Tok], span: Span, reader: &mut Reader) {
        let body = self.nasm_capture(
            reader,
            &["macro", "imacro", "rmacro", "irmacro"],
            &["endmacro", "endm"],
            span,
            "%endmacro",
        );
        let Some(body) = body else {
            return;
        };
        let args = token::trim(args);
        let Some(name_tok) = args.first().filter(|t| t.kind == Kind::Ident) else {
            self.diags.error(span, "`%macro` expects a macro name");
            return;
        };
        let name = name_tok.text.clone();
        let mut i = 1;
        let skip = |i: &mut usize| {
            while args.get(*i).is_some_and(Tok::is_space) {
                *i += 1;
            }
        };
        skip(&mut i);
        let number = |t: Option<&Tok>| {
            t.filter(|t| t.kind == Kind::Number)
                .and_then(|t| t.text.parse::<usize>().ok())
        };
        let Some(min) = number(args.get(i)) else {
            self.diags
                .error(span, "`%macro` expects a parameter count after the name");
            return;
        };
        i += 1;
        let mut max = min;
        if args.get(i).is_some_and(|t| t.is("-")) {
            i += 1;
            if args.get(i).is_some_and(|t| t.is("*")) {
                max = usize::MAX;
                i += 1;
            } else if let Some(n) = number(args.get(i)) {
                max = n;
                i += 1;
            } else {
                self.diags
                    .error(span, "`%macro` expects a maximum parameter count or `*`");
                return;
            }
        }
        let mut greedy = false;
        if args.get(i).is_some_and(|t| t.is("+")) {
            greedy = true;
            i += 1;
        }
        if args
            .get(i)
            .is_some_and(|t| t.kind == Kind::Ident && t.text.eq_ignore_ascii_case(".nolist"))
        {
            i += 1;
        }
        let rest = token::trim(&args[i.min(args.len())..]);
        let defaults = if rest.is_empty() {
            Vec::new()
        } else {
            token::split_args(rest)
        };
        let insensitive = dir.starts_with('i');
        let captures_label = body.contains("%00");
        let mac = Rc::new(MMacro {
            name: name.clone(),
            min,
            max: max.max(min),
            greedy,
            defaults,
            recursive: dir.ends_with("rmacro"),
            captures_label,
            body,
        });
        let table = if insensitive {
            self.nasm
                .immacros
                .entry(name.to_ascii_lowercase())
                .or_default()
        } else {
            self.nasm.mmacros.entry(name).or_default()
        };
        // A definition with the same parameter range replaces the old one.
        table.retain(|m| (m.min, m.max, m.greedy) != (mac.min, mac.max, mac.greedy));
        table.push(mac);
    }

    fn nasm_unmacro(&mut self, insensitive: bool, args: &[Tok], span: Span) {
        let args = token::trim(args);
        let Some(name) = args.first() else {
            self.diags.error(span, "`%unmacro` expects a macro name");
            return;
        };
        let spec = token::render(&args[1..]);
        let spec = spec.trim();
        let (min, max) = match spec.split_once('-') {
            Some((a, "*")) => (a.trim().parse().ok(), Some(usize::MAX)),
            Some((a, b)) => (a.trim().parse().ok(), b.trim().parse().ok()),
            None => (spec.parse().ok(), spec.parse().ok()),
        };
        let table = if insensitive {
            self.nasm.immacros.get_mut(&name.text.to_ascii_lowercase())
        } else {
            self.nasm.mmacros.get_mut(&name.text)
        };
        if let (Some(t), Some(min), Some(max)) = (table, min, max) {
            t.retain(|m| (m.min, m.max) != (min, max));
        }
    }

    fn nasm_rotate(&mut self, args: &[Tok], span: Span) {
        let Some(n) = self.nasm_eval(args, span, "`%rotate` count") else {
            return;
        };
        let Some(frame) = self.nasm.frames.last_mut() else {
            self.diags.error(span, "`%rotate` outside a macro call");
            return;
        };
        let count = frame.params.len() as i64;
        if count == 0 {
            self.diags
                .error(span, "`%rotate` in a macro called without parameters");
            return;
        }
        frame.rotate = (frame.rotate as i64 + n).rem_euclid(count) as usize;
    }

    /// Reads the lines of a block up to its terminator, returning their text
    /// as written. `opens` and `closes` are directive names that nest.
    fn nasm_capture(
        &mut self,
        reader: &mut Reader,
        opens: &[&str],
        closes: &[&str],
        span: Span,
        want: &str,
    ) -> Option<String> {
        let mut body = String::new();
        let mut depth = 1usize;
        while let Some(line) = self.nasm_read_line(reader) {
            reader.parser.set_offset(line.next);
            let toks = token::tokenize(&line.text);
            if let Some(first) = toks.iter().find(|t| !t.is_space())
                && first.kind == Kind::Pp
            {
                let d = first.text[1..].to_ascii_lowercase();
                if opens.contains(&d.as_str()) {
                    depth += 1;
                } else if closes.contains(&d.as_str()) {
                    depth -= 1;
                    if depth == 0 {
                        return Some(body);
                    }
                }
            }
            let file = self.sm.file(reader.parser.file());
            body.push_str(&file.src[line.start..line.next]);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        self.diags
            .error(span, format!("unterminated block, expected `{want}`"));
        None
    }

    /// Replaces a macro's parameters in a line of its body, or returns
    /// `None` if the line has none.
    fn nasm_expand_params(&mut self, toks: &[Tok], span: Span) -> Option<Vec<Tok>> {
        if !toks.iter().any(|t| t.kind == Kind::Pp) {
            return None;
        }
        let frame = self.nasm.frames.last()?;
        let n = frame.params.len();
        let param = |k: usize| -> Vec<Tok> {
            if k == 0 || k > n {
                return Vec::new();
            }
            frame.params[(k - 1 + frame.rotate) % n].clone()
        };
        let mut out = Vec::with_capacity(toks.len());
        let mut changed = false;
        let mut errors = Vec::new();
        for t in toks {
            if t.kind != Kind::Pp {
                out.push(t.clone());
                continue;
            }
            let text = t
                .text
                .strip_prefix("%{")
                .and_then(|s| s.strip_suffix('}'))
                .map_or(t.text[1..].to_string(), str::to_string);
            if let Some(local) = text.strip_prefix('%') {
                out.push(Tok::new(
                    Kind::Ident,
                    format!("..@{}.{local}", frame.unique),
                ));
                changed = true;
            } else if text == "?" {
                out.push(Tok::new(Kind::Ident, frame.invoked.clone()));
                changed = true;
            } else if text == "??" {
                out.push(Tok::new(Kind::Ident, frame.mac.name.clone()));
                changed = true;
            } else if text == "00" {
                out.extend(frame.label.iter().cloned());
                changed = true;
            } else if text == "0" {
                out.extend(number_tokens(n as i64));
                changed = true;
            } else if let Ok(k) = text.parse::<usize>() {
                out.extend(param(k));
                changed = true;
            } else if let Some((a, b)) = text.split_once(':')
                && let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>())
            {
                // `%{2:3}`, or backwards, `%{3:2}`; negative counts from the
                // end.
                let fix = |v: i64| if v < 0 { v + n as i64 + 1 } else { v };
                let (a, b) = (fix(a), fix(b));
                let step: i64 = if a <= b { 1 } else { -1 };
                let mut k = a;
                let mut first = true;
                loop {
                    if !first {
                        out.push(Tok::new(Kind::Other, ","));
                    }
                    first = false;
                    out.extend(param(k.max(0) as usize));
                    if k == b {
                        break;
                    }
                    k += step;
                }
                changed = true;
            } else if let Some(sign) = text.chars().next().filter(|c| matches!(c, '+' | '-'))
                && let Ok(k) = text[1..].parse::<usize>()
            {
                let p = param(k);
                let cc = significant(&p);
                let code = match cc.as_slice() {
                    [c] => CONDITIONS
                        .iter()
                        .find(|(name, _)| c.text.eq_ignore_ascii_case(name)),
                    _ => None,
                };
                match code {
                    Some((name, inverse)) => {
                        let s = if sign == '-' { inverse } else { name };
                        out.push(Tok::new(Kind::Ident, *s));
                    }
                    None => {
                        errors.push(format!("macro parameter `%{text}` is not a condition code"))
                    }
                }
                changed = true;
            } else {
                out.push(t.clone());
            }
        }
        for e in errors {
            self.diags.error(span, e);
        }
        changed.then_some(out)
    }

    /// Whether a line calls a multi-line macro, and with what.
    fn nasm_find_call(&mut self, toks: &[Tok], span: Span) -> Option<Call> {
        let toks = token::trim(toks);
        let first = toks.first()?;
        if first.kind != Kind::Ident {
            return None;
        }
        let (label, at) = if self.nasm_has_mmacro(&first.text) {
            (Vec::new(), 0)
        } else {
            let mut j = 1;
            while toks.get(j).is_some_and(Tok::is_space) {
                j += 1;
            }
            if toks.get(j).is_some_and(|t| t.is(":")) {
                j += 1;
                while toks.get(j).is_some_and(Tok::is_space) {
                    j += 1;
                }
            }
            // The label is `%00` without its colon, which a line of its own
            // puts back.
            match toks.get(j) {
                Some(t) if t.kind == Kind::Ident && self.nasm_has_mmacro(&t.text) => {
                    (vec![toks[0].clone()], j)
                }
                _ => return None,
            }
        };
        let name = toks[at].text.clone();
        let rest = token::trim(&toks[at + 1..]);
        let mut args = if rest.is_empty() {
            Vec::new()
        } else {
            split_macro_args(rest)
        };
        // A trailing empty argument is dropped where that is what matches,
        // as NASM does for compatibility.
        let lower = name.to_ascii_lowercase();
        let candidates: Vec<Rc<MMacro>> = self
            .nasm
            .mmacros
            .get(&name)
            .into_iter()
            .flatten()
            .chain(self.nasm.immacros.get(&lower).into_iter().flatten())
            .filter(|m| m.recursive || !self.nasm.frames.iter().any(|f| Rc::ptr_eq(&f.mac, m)))
            .cloned()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let fits = |m: &MMacro, n: usize| n >= m.min && (m.greedy || n <= m.max);
        let mut found = candidates.iter().find(|m| fits(m, args.len())).cloned();
        if found.is_none() && args.len() > 1 && args.last().is_some_and(Vec::is_empty) {
            let shorter = args.len() - 1;
            if let Some(m) = candidates.iter().find(|m| fits(m, shorter)) {
                args.pop();
                found = Some(m.clone());
            }
        }
        let Some(mac) = found else {
            self.diags.warning(
                span,
                format!(
                    "multi-line macro `{name}` exists, but not taking {} parameter{}",
                    args.len(),
                    if args.len() == 1 { "" } else { "s" }
                ),
            );
            return None;
        };
        // The greedy last parameter is the rest of the line as written.
        if mac.greedy && args.len() > mac.max {
            let rest_text = split_macro_args_greedy(rest, mac.max);
            args.truncate(mac.max - 1);
            args.push(rest_text);
        }
        let given = args.len();
        if given < mac.min + mac.defaults.len() {
            for k in given..mac.min + mac.defaults.len() {
                if k >= mac.min {
                    args.push(mac.defaults[k - mac.min].clone());
                }
            }
        }
        Some(Call {
            mac,
            invoked: name,
            args,
            label,
        })
    }

    fn nasm_has_mmacro(&self, name: &str) -> bool {
        self.nasm.mmacros.get(name).is_some_and(|v| !v.is_empty())
            || self
                .nasm
                .immacros
                .get(&name.to_ascii_lowercase())
                .is_some_and(|v| !v.is_empty())
    }

    fn nasm_call(&mut self, call: Call, span: Span) {
        if self.nasm.depth >= MAX_DEPTH {
            self.diags
                .error(span, "macro expansion nested too deeply; is it recursive?");
            return;
        }
        if !call.label.is_empty() && !call.mac.captures_label {
            let mut text = token::render(&call.label);
            if !text.ends_with(':') {
                text.push(':');
            }
            self.nasm_assemble_text(&text, span);
        }
        self.nasm.next_unique += 1;
        let frame = Frame {
            mac: call.mac.clone(),
            invoked: call.invoked.clone(),
            params: call.args,
            rotate: 0,
            label: call.label,
            unique: self.nasm.next_unique,
            reps: self.nasm.reps,
        };
        self.nasm.frames.push(frame);
        let file = self
            .sm
            .add(format!("<macro {}>", call.mac.name), call.mac.body.clone());
        self.nasm.depth += 1;
        self.assemble_file(file);
        self.nasm.depth -= 1;
        self.nasm.frames.pop();
        if self.nasm.unwind == Some(Unwind::Macro) {
            self.nasm.unwind = None;
        }
    }

    fn nasm_rep(&mut self, args: &[Tok], span: Span, reader: &mut Reader) {
        let body = self.nasm_capture(reader, &["rep"], &["endrep"], span, "%endrep");
        let Some(body) = body else {
            return;
        };
        let Some(count) = self.nasm_eval(args, span, "`%rep` count") else {
            return;
        };
        if count > MAX_REP {
            self.diags.error(
                span,
                format!("`%rep` count {count} exceeds the limit of {MAX_REP}"),
            );
            return;
        }
        if count <= 0 || body.is_empty() {
            return;
        }
        if self.nasm.depth >= MAX_DEPTH {
            self.diags
                .error(span, "`%rep` nested too deeply; is it recursive?");
            return;
        }
        let text = body.repeat(count as usize);
        let file = self.sm.add("<%rep>", text);
        self.nasm.reps += 1;
        self.nasm.depth += 1;
        self.assemble_file(file);
        self.nasm.depth -= 1;
        self.nasm.reps -= 1;
        if self.nasm.unwind == Some(Unwind::Rep) {
            self.nasm.unwind = None;
        }
    }
}

fn reader_span(asm: &Assembler, reader: &Reader) -> Span {
    let f = asm.sm.file(reader.parser.file());
    let at = f.start + reader.parser.offset() as u32;
    Span::new(at, at)
}

/// A multi-line macro call.
pub(crate) struct Call {
    mac: Rc<MMacro>,
    invoked: String,
    args: Vec<Vec<Tok>>,
    label: Vec<Tok>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CondOp {
    If,
    Elif,
    Else,
    Endif,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CondKind {
    Expr,
    Def,
    Macro,
    Idn,
    Idni,
    Num,
    Str,
    Id,
    Token,
    Empty,
    Ctx,
    Env,
}

#[derive(Copy, Clone, Debug)]
struct Condition {
    op: CondOp,
    kind: CondKind,
    negate: bool,
}

/// Reads a conditional directive's name: `if`, `elifndef`, `ifnidni`...
fn parse_condition(name: &str) -> Option<Condition> {
    let plain = |op| {
        Some(Condition {
            op,
            kind: CondKind::Expr,
            negate: false,
        })
    };
    match name {
        "else" => return plain(CondOp::Else),
        "endif" => return plain(CondOp::Endif),
        _ => {}
    }
    let (op, rest) = match name.strip_prefix("elif") {
        Some(r) => (CondOp::Elif, r),
        None => (CondOp::If, name.strip_prefix("if")?),
    };
    let kind = |s: &str| {
        Some(match s {
            "" => CondKind::Expr,
            "def" => CondKind::Def,
            "macro" => CondKind::Macro,
            "idn" => CondKind::Idn,
            "idni" => CondKind::Idni,
            "num" => CondKind::Num,
            "str" => CondKind::Str,
            "id" => CondKind::Id,
            "token" => CondKind::Token,
            "empty" => CondKind::Empty,
            "ctx" => CondKind::Ctx,
            "env" => CondKind::Env,
            _ => return None,
        })
    };
    if let Some(k) = kind(rest) {
        return Some(Condition {
            op,
            kind: k,
            negate: false,
        });
    }
    let k = kind(rest.strip_prefix('n')?)?;
    Some(Condition {
        op,
        kind: k,
        negate: true,
    })
}

fn builtin_name(name: &str) -> bool {
    matches!(
        name,
        "__?LINE?__"
            | "__?FILE?__"
            | "__?BITS?__"
            | "__?PTR?__"
            | "__?PASS?__"
            | "__?OUTPUT_FORMAT?__"
            | "__?NASM_MAJOR?__"
            | "__?NASM_MINOR?__"
            | "__?NASM_SUBMINOR?__"
            | "__?NASM_PATCHLEVEL?__"
            | "__?NASM_VERSION_ID?__"
            | "__?NASM_VER?__"
    )
}

/// The tokens that are not blanks.
fn significant(toks: &[Tok]) -> Vec<Tok> {
    toks.iter().filter(|t| !t.is_space()).cloned().collect()
}

/// A number as a macro body: a minus sign is a token of its own.
fn number_tokens(v: i64) -> Vec<Tok> {
    if v < 0 {
        vec![
            Tok::new(Kind::Other, "-"),
            Tok::new(Kind::Number, v.unsigned_abs().to_string()),
        ]
    } else {
        vec![Tok::new(Kind::Number, v.to_string())]
    }
}

/// Bytes as a string token, in whichever quotes can hold them as written.
fn quote(bytes: &[u8]) -> Tok {
    let text = String::from_utf8_lossy(bytes);
    let plain = |q: char| !text.contains(q) && !text.contains('\n');
    if plain('\'') {
        return Tok::new(Kind::Str, format!("'{text}'"));
    }
    if plain('"') {
        return Tok::new(Kind::Str, format!("\"{text}\""));
    }
    let mut s = String::from("`");
    for &b in bytes {
        match b {
            b'`' => s.push_str("\\`"),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s.push('`');
    Tok::new(Kind::Str, s)
}

/// Splits a single-line macro call's arguments at commas outside parentheses
/// and braces, taking braces off an argument they enclose.
fn split_call_args(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut out = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let (mut paren, mut brace) = (0i32, 0i32);
    for t in toks {
        if t.is("(") && brace == 0 {
            paren += 1;
        } else if t.is(")") && brace == 0 {
            paren -= 1;
        } else if t.is("{") {
            brace += 1;
        } else if t.is("}") {
            brace -= 1;
        } else if t.is(",") && paren == 0 && brace == 0 {
            out.push(token::split_args(&cur).concat_with_commas());
            cur.clear();
            continue;
        }
        cur.push(t.clone());
    }
    out.push(token::split_args(&cur).concat_with_commas());
    out
}

/// Splits a multi-line macro call's arguments.
fn split_macro_args(toks: &[Tok]) -> Vec<Vec<Tok>> {
    token::split_args(toks)
}

/// The tokens of a greedy parameter: everything from argument `index` (1
/// based) on, as written.
fn split_macro_args_greedy(toks: &[Tok], index: usize) -> Vec<Tok> {
    if index <= 1 {
        return token::trim(toks).to_vec();
    }
    let mut commas = 0;
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if t.is("{") {
            depth += 1;
        } else if t.is("}") {
            depth -= 1;
        } else if t.is(",") && depth == 0 {
            commas += 1;
            if commas == index - 1 {
                return token::trim(&toks[i + 1..]).to_vec();
            }
        }
    }
    Vec::new()
}

trait ConcatWithCommas {
    fn concat_with_commas(self) -> Vec<Tok>;
}

impl ConcatWithCommas for Vec<Vec<Tok>> {
    /// Undoes a split: the pieces joined with commas, which is how a
    /// single-line macro argument is kept whole while its braces go.
    fn concat_with_commas(self) -> Vec<Tok> {
        let mut out = Vec::new();
        for (i, p) in self.into_iter().enumerate() {
            if i > 0 {
                out.push(Tok::new(Kind::Other, ","));
            }
            out.extend(p);
        }
        out
    }
}
