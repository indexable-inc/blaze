//! WHY a census row is left out: the evidence under a `SPLIT` row of the build census
//! (blaze-real examples/tempo_build.rs), read from the row's typed refusal ([`LeftOut`]) and
//! the names library's public tables, so a lane reads the cause instead of guessing it.
//!
//! One block a row, at most [`MOST_LINES`] lines and then `... N more` (rule 18), each line cut
//! at [`MOST_CHARS`] characters. Every list is sorted (by place, then key) before it is
//! printed: no map order reaches the output (rule 25). The four cases:
//!
//! - (a) a refused impl header whose path did not resolve (`Gap::RefusedImpl`, `Resolve(Path(
//!   PathError { .. why: NotFound }))`, "the header's path as written: `X`"): the modules the
//!   path was read in; every macro call filed there that was refused or made no text, with what
//!   its path resolves to; every call of the crate whose output declares the missing name, and
//!   every attribute or derive call on an item of that name; every unresolved `use` there; every
//!   item of that name in the crate's files and whether a `cfg` removed it.
//! - (b) a chain of constants and the bodies they need (`ConstPath`, `a body it needs`,
//!   `Nested`): one link a line, innermost first, each key it names shown by its path.
//! - (c) a member whose head was refused as `Unsupported(..)` (`lower: Head`): the member's
//!   line with the refused range marked `[[..]]`, and the mark of the token there.
//! - (d) any other: the innermost refusal and the item's `file:line`.
//!
//! What the public tables do not hold is printed as `not recorded: <what>`, naming the record
//! that would answer it.

use core::fmt::{self, Write as _};

use blaze_id::ItemKey as LibKey;
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::resolve::{At, CrateNames, ExpandNote, Names, NamesNote};
use blaze_scope::task::item_attrs;
use blaze_surface::{AttrKind, CallShape, ItemDetail, Namespace, PathKind, ScopeError};

use crate::front2::body::BodyError;
use crate::front2::check::CheckFail;
use crate::front2::heads::HeadError;
use crate::front2::item_shown::ItemShown;
use crate::front2::item_site::member_marks;
use crate::front2::keys::checker_of;
use crate::front2::program::{Front2Refusal, LeftAt, LeftOut};
use crate::front2::types::Why as TyWhy;
use crate::front2::world_items::Gap;

/// The most lines a block prints before `... N more`.
pub const MOST_LINES: usize = 20;
/// The most characters a line keeps; a longer one ends in `...`.
pub const MOST_CHARS: usize = 400;
/// The most keys of a chain looked up by their shown text (each lookup reads every item).
const MOST_KEYS: usize = 8;
/// The most names an expansion's declarations list on its line.
const MOST_DECLS: usize = 12;

/// The markers a refusal's text joins one link of a chain to the next by, as the front end
/// writes them (consts.rs, const_at.rs): the text before a marker needed what comes after it.
const LINKS: [&str; 3] = [": a body it needs: ", " an array length names: ", ": its head was refused: "];

/// One row's evidence: at most [`MOST_LINES`] lines, and how many more were found.
pub struct WhyBlock<'a> {
    lines: AVec<'a, AString<'a>>,
    more: usize,
    alloc: Alloc<'a>,
}

impl<'a> WhyBlock<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> WhyBlock<'a> {
        WhyBlock { lines: AVec::with_capacity_in(MOST_LINES, alloc), more: 0, alloc }
    }

    /// Adds one line (a newline in it written as a space, cut at [`MOST_CHARS`]), or counts it
    /// past [`MOST_LINES`].
    pub fn line(&mut self, args: fmt::Arguments<'_>) {
        if self.lines.len() >= MOST_LINES {
            self.more += 1;
            return;
        }
        let mut said = AString::new_in(self.alloc);
        if said.write_fmt(args).is_err() {
            said.truncate(0);
            said.push_str("(a line of this block did not format)");
        }
        self.lines.push(flat_cut(said.as_str(), MOST_CHARS, self.alloc));
    }

    pub fn lines(&self) -> &[AString<'a>] {
        &self.lines
    }

    /// The lines found past [`MOST_LINES`].
    pub fn more(&self) -> usize {
        self.more
    }

    /// `WHY` and the lines under it, each after `indent`; then `... N more` when some were cut.
    pub fn write_to(&self, out: &mut dyn fmt::Write, indent: &str) -> fmt::Result {
        writeln!(out, "{indent}WHY")?;
        for l in self.lines.iter() {
            writeln!(out, "{indent}  {}", l.as_str())?;
        }
        if self.more > 0 {
            writeln!(out, "{indent}  ... {} more", self.more)?;
        }
        Ok(())
    }
}

/// `text` on one line (each newline a space), cut at `most` characters with `...` after.
pub fn flat_cut<'a>(text: &str, most: usize, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(text.len().min(most + 3), alloc);
    for (n, c) in text.chars().enumerate() {
        if n == most {
            out.push_str("...");
            break;
        }
        out.push(if c == '\n' { ' ' } else { c });
    }
    out
}

/// The links of a refusal's chain (`text` cut at each of [`LINKS`]), INNERMOST FIRST: the
/// last thing needed leads. A text with no marker is one link.
pub fn chain_links<'t, 'a>(text: &'t str, alloc: Alloc<'a>) -> AVec<'a, &'t str> {
    let mut links: AVec<'a, &'t str> = AVec::with_capacity_in(4, alloc);
    let mut rest = text;
    loop {
        let next = LINKS.iter().filter_map(|m| rest.find(m).map(|at| (at, m.len()))).min();
        match next {
            Some((at, len)) => {
                links.push(&rest[..at]);
                rest = &rest[at + len..];
            }
            None => {
                links.push(rest);
                break;
            }
        }
    }
    links.as_mut_slice().reverse();
    links
}

/// The line of `text` that holds `start`, the bytes `start..end` (cut at the line's end)
/// between `[[` and `]]`, and that line's number (1-based); `None` when the range is not in
/// `text` on character boundaries.
pub fn marked_line<'a>(text: &str, start: usize, end: usize, alloc: Alloc<'a>) -> Option<(usize, AString<'a>)> {
    if start > end || end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return None;
    }
    let line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |at| start + at);
    let end = end.min(line_end);
    let mut out = AString::with_capacity_in(line_end - line_start + 4, alloc);
    out.push_str(text[line_start..start].trim_start());
    out.push_str("[[");
    out.push_str(&text[start..end]);
    out.push_str("]]");
    out.push_str(&text[end..line_end]);
    Some((1 + text[..start].matches('\n').count(), out))
}

/// The path written in a refused header's why: what follows "the header's path as written: `"
/// (header_written.rs `WRITTEN`) up to its closing back-quote.
pub fn written_path(why: &str) -> Option<&str> {
    let at = why.find(crate::front2::header_written::WRITTEN)? + crate::front2::header_written::WRITTEN.len();
    let rest = why[at..].strip_prefix('`')?;
    Some(&rest[..rest.rfind('`')?])
}

/// The failing segment of a `PathError`'s `Debug` text (`.., at: 2, ..`).
pub fn failed_segment(why: &str) -> Option<usize> {
    let at = why.find("PathError {")?;
    let rest = &why[at..];
    let n = &rest[rest.find(" at: ")? + 5..];
    let digits = n.find(|c: char| !c.is_ascii_digit()).unwrap_or(n.len());
    n[..digits].parse().ok()
}

/// The segments of a written path, each without its generic arguments (`a::B<T>` is `a`, `B`).
pub fn plain_segments<'t, 'a>(written: &'t str, alloc: Alloc<'a>) -> AVec<'a, &'t str> {
    let mut out: AVec<'a, &'t str> = AVec::with_capacity_in(4, alloc);
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut cut: Option<usize> = None;
    let b = written.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => {
                if depth == 0 && cut.is_none() {
                    cut = Some(i);
                }
                depth += 1;
            }
            b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && b.get(i + 1) == Some(&b':') => {
                let seg = written[start..cut.unwrap_or(i)].trim();
                if !seg.is_empty() {
                    out.push(seg);
                }
                i += 2;
                start = i;
                cut = None;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let seg = written[start..cut.unwrap_or(b.len())].trim();
    if !seg.is_empty() {
        out.push(seg);
    }
    out
}

/// Each `ItemKey(..)` token of `text`, in order, each once.
pub fn key_tokens<'t, 'a>(text: &'t str, alloc: Alloc<'a>) -> AVec<'a, &'t str> {
    let mut out: AVec<'a, &'t str> = AVec::with_capacity_in(4, alloc);
    let mut rest = text;
    let mut base = 0usize;
    while let Some(at) = rest.find("ItemKey(") {
        let Some(close) = rest[at..].find(')') else { break };
        let token = &text[base + at..base + at + close + 1];
        if !out.contains(&token) {
            out.push(token);
        }
        base += at + close + 1;
        rest = &text[base..];
    }
    out
}

/// Whether an item text (a derive's or an attribute's input) declares an item named `name`.
pub fn declares_named(input: &str, name: &str) -> bool {
    const KEYWORDS: [&str; 9] = ["struct ", "enum ", "union ", "trait ", "type ", "fn ", "mod ", "const ", "static "];
    KEYWORDS.iter().any(|k| {
        let mut rest = input;
        while let Some(at) = rest.find(k) {
            let after = rest[at + k.len()..].trim_start();
            if let Some(tail) = after.strip_prefix(name)
                && !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            {
                return true;
            }
            rest = &rest[at + k.len()..];
        }
        false
    })
}

// ---------------------------------------------------------------------------------------------
// The evidence, over the names.

/// The evidence for `row`, a fn the survey left out (module docs: cases a to d).
pub fn why_of<'n, 'a>(names: &'n Names<'n>, row: &LeftOut, alloc: Alloc<'a>) -> WhyBlock<'a> {
    let mut block = WhyBlock::new_in(alloc);
    if let Front2Refusal::Check(CheckFail::Gap { gaps, .. }) = &row.refusal
        && let Some(Gap::RefusedImpl { impl_, why, .. }) = gaps.first()
        && why.contains("Resolve(Path(PathError")
    {
        header_path(names, *impl_, why, &mut block, alloc);
        return block;
    }
    if let Front2Refusal::Lower(BodyError::Head { item, error: HeadError::Item(_, why) }) = &row.refusal {
        match why {
            TyWhy::Program(said, _) if said.contains("its head was refused") => {
                member_head(names, *item, said, &mut block, alloc);
                return block;
            }
            TyWhy::Unsupported(said, r) | TyWhy::UnsupportedIn(said, _, r) => {
                let text = names.item_text(*item).map(|t| t.text);
                block.line(format_args!("the item {}: its head was refused: {said}", ItemShown::of_lib(names, *item)));
                refused_range(names, *item, None, text, r.start as usize, r.end as usize, &mut block, alloc);
                return block;
            }
            _ => {}
        }
    }
    if row.whole.contains("ConstPath(") || row.whole.contains("Nested {") || row.whole.contains(LINKS[0]) {
        const_chain(names, &row.whole, &mut block, alloc);
        return block;
    }
    other(names, row, &mut block, alloc);
    block
}

/// Case (a): an impl's header path that did not resolve.
fn header_path<'n>(names: &'n Names<'n>, impl_: LibKey, why: &str, block: &mut WhyBlock<'_>, alloc: Alloc<'_>) {
    let Some(written) = written_path(why) else {
        block.line(format_args!("not recorded: the header's path as written (the refusal's text has no \"{}\")", crate::front2::header_written::WRITTEN));
        return;
    };
    let segments = plain_segments(written, alloc);
    let at = failed_segment(why);
    let name = at.and_then(|i| segments.get(i).copied()).or(segments.last().copied()).unwrap_or(written);
    block.line(format_args!("the impl {} names `{written}`; it stops at segment {} `{name}`", ItemShown::of_lib(names, impl_), at.map_or(-1, |i| i as i64)));
    let Some(krate) = names.crate_of(impl_) else {
        block.line(format_args!("not recorded: the crate of impl {impl_:x}"));
        return;
    };
    let c = names.krate(krate);
    // The module the path was read in: the PathError's, which the gap holds as text only.
    let module_token = why.find("PathError { module: ").and_then(|at| key_tokens(&why[at..], alloc).first().copied());
    let mut module = None;
    if let Some(token) = module_token {
        let mut shown = AString::new_in(alloc);
        let mut found: AVec<'_, LibKey> = AVec::with_capacity_in(1, alloc);
        for m in names.modules(krate) {
            shown.truncate(0);
            if write!(shown, "{:?}", m.key).is_ok() && shown.as_str() == token {
                found.push(m.key);
            }
        }
        // An `ItemKey` prints its first 12 hex digits, which are its crate's prefix
        // (`ItemKey::crate_prefix`): every module of the crate prints the same. Census g033201
        // took the smallest of them (a block module of another file) and said the path was read
        // there. Only a token one module alone prints names it.
        if found.len() == 1 {
            module = found.first().copied();
        } else if found.len() > 1 {
            block.line(format_args!("the module the path was read in prints as `{token}`, as {} modules of the crate do: taken as the impl's own", found.len()));
        }
    }
    let module = match module.or_else(|| names.module_of(impl_)) {
        Some(m) => m,
        None => {
            block.line(format_args!("not recorded: the module of impl {impl_:x} (no item text)"));
            return;
        }
    };
    if module_token.is_some() && Some(module) != names.module_of(impl_) {
        block.line(format_args!("the path is read in module `{}`, not the impl's own", c.path_of(module).unwrap_or("?")));
    }
    let chain: &[LibKey] = match c.chains.get(&module) {
        Some(ch) => ch.as_slice(),
        None => core::slice::from_ref(&module),
    };
    let mut shown_chain = AString::new_in(alloc);
    for (i, m) in chain.iter().enumerate() {
        if i > 0 {
            shown_chain.push_str(" < ");
        }
        shown_chain.push_str(c.path_of(*m).unwrap_or("?"));
    }
    block.line(format_args!("modules read: {}", shown_chain.as_str()));
    let notes = expand_notes(c, alloc);

    // (i) the calls filed in those modules that were refused or made no text, by place.
    let mut calls: AVec<'_, (usize, u32, u32, LibKey)> = AVec::with_capacity_in(16, alloc);
    for (&key, call) in c.tables.calls.iter() {
        if let Some(depth) = chain.iter().position(|m| *m == call.module) {
            calls.push((depth, call.loc.file, call.loc.offset, key));
        }
    }
    calls.as_mut_slice().sort_unstable();
    let mut expanded = 0usize;
    let mut inert = 0usize;
    for &(_, _, _, key) in calls.iter() {
        let Some(call) = c.tables.calls.get(&key) else { continue };
        // A built-in attribute (`repr`, `inline`, `automatically_derived`, ..) is inert: it is
        // never expanded and never resolves as a macro, so it names no cause (census
        // g033201 listed 200 of them as "NEVER MADE" under one row).
        if matches!(call.kind, CallShape::Attr) && blaze_surface::builtin_attrs::BUILTIN_ATTRS.binary_search(&call.name).is_ok() {
            inert += 1;
            continue;
        }
        let state = match c.expansions.get(&key) {
            None => "NEVER MADE (no expansion filed)",
            Some(e) if e.refused.is_some() => "REFUSED",
            Some(e) if e.made.is_none() => "MADE NO TEXT",
            Some(_) => {
                expanded += 1;
                continue;
            }
        };
        let mut line = AString::new_in(alloc);
        let _ = write!(line, "call `{}` ({}) at ", call.name, shape(call.kind));
        loc_line(c, call.loc, &mut line);
        let _ = write!(line, ": {state}");
        if let Some(r) = c.expansions.get(&key).and_then(|e| e.refused.as_ref()) {
            let _ = write!(line, ": {}", r.why.text());
        }
        line.push_str("; its path ");
        resolved_macro(names, call.module, call.name, &mut line, alloc);
        block.line(format_args!("{}", line.as_str()));
        for note in notes_of(&notes, key) {
            block.line(format_args!("  note: {note:?}"));
        }
    }
    block.line(format_args!("{expanded} other calls there expanded; {inert} built-in attributes (inert) not listed"));

    // (ii) every call of the crate whose output declares `name`, and every attribute or derive
    // call on an item of that name (its input declares it).
    let mut writers: AVec<'_, (u32, u32, LibKey)> = AVec::with_capacity_in(4, alloc);
    for (&key, call) in c.tables.calls.iter() {
        let on_item = call.kind != CallShape::FnLike && declares_named(call.input, name);
        let made = c.expansions.get(&key).and_then(|e| e.made).is_some_and(|m| outline_declares(m.text, &m.outline, name));
        if on_item || made {
            writers.push((call.loc.file, call.loc.offset, key));
        }
    }
    writers.as_mut_slice().sort_unstable();
    if writers.is_empty() {
        block.line(format_args!("no macro call of the crate declares `{name}` or is written on an item of that name"));
    }
    for &(_, _, key) in writers.iter() {
        let Some(call) = c.tables.calls.get(&key) else { continue };
        let mut line = AString::new_in(alloc);
        let _ = write!(line, "`{}` ({}) in `{}` at ", call.name, shape(call.kind), c.path_of(call.module).unwrap_or("?"));
        loc_line(c, call.loc, &mut line);
        match c.expansions.get(&key) {
            None => line.push_str(": never made"),
            Some(e) => match (e.refused.as_ref(), e.made) {
                (Some(r), _) => {
                    let _ = write!(line, ": refused: {}", r.why.text());
                }
                (None, None) => line.push_str(": ran, made no text"),
                (None, Some(m)) => {
                    line.push_str(": ran; declares ");
                    decls_of(m.text, &m.outline, &mut line);
                }
            },
        }
        block.line(format_args!("{}", line.as_str()));
    }

    // (iii) the unresolved imports, macro paths and refused calls of those modules.
    for m in chain.iter() {
        let path = c.path_of(*m).unwrap_or("?");
        if let Some(failed) = c.failed.get(m) {
            block.line(format_args!("module `{path}`: its task failed: {failed:?}"));
        }
        let Some(done) = c.done.get(m) else {
            if c.failed.get(m).is_none() {
                block.line(format_args!("not recorded: module `{path}` has no finished scope (CrateNames::done)"));
            }
            continue;
        };
        for e in done.scope.errors.iter() {
            match e {
                ScopeError::Import { path: p, segment, why, .. } => {
                    block.line(format_args!("module `{path}`: unresolved use `{}` at segment {segment}: {why:?}", path_text(p.kind, &p.segments, alloc).as_str()));
                }
                ScopeError::Macro { path: p, segment, why, .. } => {
                    block.line(format_args!("module `{path}`: unresolved macro path `{}` at segment {segment}: {why:?}", path_text(p.kind, &p.segments, alloc).as_str()));
                }
                ScopeError::Refused { name: n, why, .. } => {
                    block.line(format_args!("module `{path}`: refused call `{n}`: {}", why.text()));
                }
                _ => {}
            }
        }
    }

    // (iv) every item of that name in the crate's files, and whether a cfg removed it.
    let mut found = 0usize;
    for (fi, f) in c.files.iter().enumerate() {
        for item in f.outline.items() {
            let Some(r) = item.name else { continue };
            if !named_kind(item.detail) || item.text(f.text, r).trim_start_matches("r#") != name {
                continue;
            }
            found += 1;
            let attrs = item_attrs(&c.cfg, &f.outline, item, f.text, alloc);
            let line_no = 1 + f.text[..(item.range.start as usize).min(f.text.len())].matches('\n').count();
            let mut cfgs = AString::new_in(alloc);
            for a in f.outline.attrs(item) {
                if matches!(a.kind, AttrKind::Cfg | AttrKind::CfgAttr) {
                    cfgs.push(' ');
                    cfgs.push_str(item.text(f.text, a.range));
                }
            }
            let state = if attrs.cfg_ok { "cfg on" } else { "REMOVED by cfg" };
            block.line(format_args!("`{name}` declared at {}:{line_no} (file {fi}): {state}{}", f.shown, cfgs.as_str()));
        }
    }
    if found == 0 {
        block.line(format_args!("no item named `{name}` in the crate's {} files (an expansion's are above)", c.files.len()));
    }
}

/// Case (c): a member whose head was refused, from body.rs `refused_head`'s words.
fn member_head<'n>(names: &'n Names<'n>, container: LibKey, said: &str, block: &mut WhyBlock<'_>, alloc: Alloc<'_>) {
    let Some(token) = said.find("member ").and_then(|at| key_tokens(&said[at..], alloc).first().copied()) else {
        block.line(format_args!("not recorded: the member's key (the refusal reads: {said})"));
        return;
    };
    let inner = said.find("Unsupported(").map_or(said, |at| &said[at..]);
    block.line(format_args!("member {token} of {}: {inner}", ItemShown::of_lib(names, container)));
    let Some(range) = last_range(inner) else {
        block.line(format_args!("not recorded: the refused range (no `Range {{ start, end }}` in the refusal)"));
        return;
    };
    let mut shown = AString::new_in(alloc);
    let member = names.members(container).and_then(|ms| {
        ms.iter().find(|m| {
            shown.truncate(0);
            write!(shown, "{:?}", checker_of(m.key)).is_ok() && shown.as_str() == token
        })
    });
    let Some(m) = member else {
        block.line(format_args!("not recorded: the member {token} among {}'s members (Names::members)", ItemShown::of_lib(names, container)));
        return;
    };
    refused_range(names, container, Some(m.written_by), Some(m.text), range.0, range.1, block, alloc);
}

/// The refused range of a member's (or an item's) text marked, and the mark of its token.
#[expect(clippy::too_many_arguments, reason = "one call per case; the parts are the refusal's")]
fn refused_range<'n>(names: &'n Names<'n>, owner: LibKey, written_by: Option<Option<LibKey>>, text: Option<&str>, start: usize, end: usize, block: &mut WhyBlock<'_>, alloc: Alloc<'_>) {
    let Some(text) = text else {
        block.line(format_args!("not recorded: the text of {owner:x}"));
        return;
    };
    match marked_line(text, start, end, alloc) {
        Some((n, line)) => block.line(format_args!("line {n} of its text: {}", line.as_str())),
        None => block.line(format_args!("not recorded: the range {start}..{end} is not in its text ({} bytes)", text.len())),
    }
    let Some(krate) = names.crate_of(owner) else { return };
    let c = names.krate(krate);
    let by = match written_by {
        Some(by) => by,
        None => match names.place(owner).map(|p| p.at) {
            Some(At::Expansion(call)) => Some(call),
            _ => None,
        },
    };
    if let Some(call) = by {
        let mut line = AString::new_in(alloc);
        match c.tables.calls.get(&call) {
            Some(cs) => {
                let _ = write!(line, "written by `{}` ({}) at ", cs.name, shape(cs.kind));
                loc_line(c, cs.loc, &mut line);
            }
            None => {
                let _ = write!(line, "written by call {call:x} (not in the crate's call table)");
            }
        }
        block.line(format_args!("{}", line.as_str()));
    }
    let marks = match written_by {
        Some(w) => member_marks(names, owner, w, text),
        None => crate::front2::item_site::item_marks(names, owner, text),
    };
    match marks {
        Err(lost) => block.line(format_args!("its marks: lost: {lost:?}")),
        Ok(None) => block.line(format_args!("its marks: none: the text is as written in a file, every token call-site at its module")),
        Ok(Some(marks)) => match text.get(start..end) {
            None => block.line(format_args!("not recorded: the token at {start}..{end}")),
            Some(token) => match marks.mark_of(token) {
                Ok(Some(mark)) => block.line(format_args!("the mark of `{token}`: {mark:?}")),
                Ok(None) => block.line(format_args!("the mark of `{token}`: no row starts there (written at the call, as in its text)")),
                Err(miss) => block.line(format_args!("the mark of `{token}`: {miss:?}")),
            },
        },
    }
}

/// Case (b): a chain of constants and bodies, innermost first, each key by its path.
fn const_chain<'n>(names: &'n Names<'n>, whole: &str, block: &mut WhyBlock<'_>, alloc: Alloc<'_>) {
    let links = chain_links(whole, alloc);
    for (i, link) in links.iter().enumerate() {
        let role = if i == 0 { "innermost" } else { "needed by" };
        block.line(format_args!("{}. {role}: {link}", i + 1));
    }
    let mut tokens = key_tokens(whole, alloc);
    tokens.truncate(MOST_KEYS);
    for (token, key) in keys_by_shown(names, &tokens, alloc).iter() {
        let mut line = AString::new_in(alloc);
        let _ = write!(line, "{token} is {} at ", ItemShown::of_lib(names, *key));
        place_line(names, *key, &mut line);
        block.line(format_args!("{}", line.as_str()));
    }
}

/// Case (d): the innermost refusal and the item's place.
fn other<'n>(names: &'n Names<'n>, row: &LeftOut, block: &mut WhyBlock<'_>, alloc: Alloc<'_>) {
    let links = chain_links(&row.whole, alloc);
    block.line(format_args!("innermost: {}", links.first().copied().unwrap_or("")));
    let key = match row.at {
        Some(LeftAt::Lib(k)) => Some(k),
        Some(LeftAt::Item(k) | LeftAt::In { item: k, .. }) => {
            let mut token = AString::new_in(alloc);
            let _ = write!(token, "{k:?}");
            keys_by_shown(names, &[token.as_str()], alloc).first().map(|(_, k)| *k)
        }
        None => None,
    };
    match key {
        Some(k) => {
            let mut line = AString::new_in(alloc);
            let _ = write!(line, "the item {} at ", ItemShown::of_lib(names, k));
            place_line(names, k, &mut line);
            if let Some(LeftAt::In { range, .. }) = row.at {
                let _ = write!(line, " (bytes {}..{} of its text)", range.start, range.end);
            }
            block.line(format_args!("{}", line.as_str()));
        }
        None => block.line(format_args!("not recorded: the item's place (the row's `at` is {:?})", row.at)),
    }
}

// ---------------------------------------------------------------------------------------------
// Small readers.

fn shape(kind: CallShape) -> &'static str {
    match kind {
        CallShape::FnLike => "fn-like",
        CallShape::Attr => "attribute",
        CallShape::Derive => "derive",
    }
}

/// Whether an outline entry of this kind is an item a path's segment can name.
fn named_kind(detail: ItemDetail) -> bool {
    matches!(
        detail,
        ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union | ItemDetail::Trait | ItemDetail::TraitAlias | ItemDetail::TypeAlias | ItemDetail::Mod { .. } | ItemDetail::MacroRules { .. } | ItemDetail::MacroDef
    )
}

/// Whether an expansion's outline holds an item named `name` at its top.
fn outline_declares(text: &str, outline: &blaze_file_items::Outline<'_>, name: &str) -> bool {
    outline.items().iter().any(|it| it.parent.is_none() && it.name.is_some_and(|r| it.text(text, r).trim_start_matches("r#") == name))
}

/// The names an expansion declares at its top, at most [`MOST_DECLS`].
fn decls_of(text: &str, outline: &blaze_file_items::Outline<'_>, out: &mut AString<'_>) {
    let mut n = 0usize;
    let mut unnamed = 0usize;
    for it in outline.items().iter().filter(|it| it.parent.is_none()) {
        match it.name {
            Some(r) if n < MOST_DECLS => {
                if n > 0 {
                    out.push_str(", ");
                }
                out.push_str(it.text(text, r));
                n += 1;
            }
            Some(_) => n += 1,
            None => unnamed += 1,
        }
    }
    if n > MOST_DECLS {
        let _ = write!(out, " and {} more", n - MOST_DECLS);
    }
    if n == 0 {
        out.push_str("no named item");
    }
    let _ = write!(out, " ({unnamed} unnamed: impls, uses, calls)");
}

/// `file:line` of a call's place.
fn loc_line(c: &CrateNames<'_>, loc: blaze_scope::task::Loc, out: &mut AString<'_>) {
    match c.line_of(loc) {
        Some((f, line)) => {
            let _ = write!(out, "{}:{line}", f.shown);
        }
        None => {
            let _ = write!(out, "file {} offset {}", loc.file, loc.offset);
        }
    }
}

/// `file:line` of a library item, or the call whose expansion holds it.
fn place_line(names: &Names<'_>, key: LibKey, out: &mut AString<'_>) {
    let Some(place) = names.place(key) else {
        out.push_str("(no place)");
        return;
    };
    let c = names.krate(place.krate);
    match place.at {
        At::File(file) => match c.files.get(file as usize) {
            Some(f) => {
                let start = f.outline.items().get(place.item.at()).map_or(0, |it| it.range.start as usize).min(f.text.len());
                let _ = write!(out, "{}:{}", f.shown, 1 + f.text[..start].matches('\n').count());
            }
            None => {
                let _ = write!(out, "(file {file} of crate {})", c.name);
            }
        },
        At::Expansion(call) => match c.tables.calls.get(&call) {
            Some(cs) => {
                let _ = write!(out, "the expansion of `{}` at ", cs.name);
                loc_line(c, cs.loc, out);
            }
            None => {
                let _ = write!(out, "the expansion of call {call:x}");
            }
        },
    }
}

/// What a macro path written in `module` resolves to, or where it stops.
fn resolved_macro<'n>(names: &'n Names<'n>, module: LibKey, written: &str, out: &mut AString<'_>, alloc: Alloc<'_>) {
    let (kind, rest) = match written.strip_prefix("::") {
        Some(rest) => (PathKind::Abs, rest),
        None => match written.strip_prefix("crate::") {
            Some(rest) => (PathKind::Crate, rest),
            None => (PathKind::Plain, written),
        },
    };
    let segments = plain_segments(rest, alloc);
    let got = blaze_scope::names::path(names, module, Namespace::Macros, blaze_scope::names::Written { kind, segments: segments.as_slice() }, alloc);
    match got {
        Ok(r) if r.consumed == segments.len() => {
            let _ = write!(out, "resolves to {:?}", r.def);
        }
        Ok(r) => {
            let _ = write!(out, "resolves {} of {} segments, to {:?}", r.consumed, segments.len(), r.def);
        }
        Err(e) => {
            let _ = write!(out, "does not resolve: {e}");
        }
    }
}

/// `a::b` with its kind's prefix.
fn path_text<'a>(kind: PathKind, segments: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    match kind {
        PathKind::Plain => {}
        PathKind::Crate => out.push_str("crate::"),
        PathKind::SelfMod => out.push_str("self::"),
        PathKind::Super(n) => {
            for _ in 0..n {
                out.push_str("super::");
            }
        }
        PathKind::Abs => out.push_str("::"),
        _ => {
            let _ = write!(out, "{kind:?}::");
        }
    }
    for (i, s) in segments.iter().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(s);
    }
    out
}

/// The crate's expansion notes by call key, sorted.
fn expand_notes<'c, 'a>(c: &'c CrateNames<'_>, alloc: Alloc<'a>) -> AVec<'a, (LibKey, &'c ExpandNote<'c>)>
where
    'c: 'a,
{
    let mut out: AVec<'a, (LibKey, &'c ExpandNote<'c>)> = AVec::with_capacity_in(16, alloc);
    for n in c.notes.iter() {
        if let NamesNote::Expand { call, note, .. } = n {
            out.push((*call, note));
        }
    }
    out.as_mut_slice().sort_by_key(|(k, _)| *k);
    out
}

/// The notes of `call` in a list [`expand_notes`] made.
fn notes_of<'l, 'c>(notes: &'l [(LibKey, &'c ExpandNote<'c>)], call: LibKey) -> impl Iterator<Item = &'c ExpandNote<'c>> + 'l {
    let start = notes.partition_point(|(k, _)| *k < call);
    notes[start..].iter().take_while(move |(k, _)| *k == call).map(|(_, n)| *n)
}

/// The library key each token (`ItemKey(..)`, a key's `Debug`, library or checker) names, by
/// reading every item and module of every crate once; a token two keys show as is listed twice.
fn keys_by_shown<'t, 'a>(names: &Names<'_>, tokens: &[&'t str], alloc: Alloc<'a>) -> AVec<'a, (&'t str, LibKey)> {
    let mut out: AVec<'a, (&'t str, LibKey)> = AVec::with_capacity_in(tokens.len(), alloc);
    if tokens.is_empty() {
        return out;
    }
    let mut shown = AString::new_in(alloc);
    let mut check = |key: LibKey, out: &mut AVec<'a, (&'t str, LibKey)>| {
        for form in 0..2 {
            shown.truncate(0);
            let ok = match form {
                0 => write!(shown, "{key:?}"),
                _ => write!(shown, "{:?}", checker_of(key)),
            };
            if ok.is_ok()
                && let Some(t) = tokens.iter().find(|t| **t == shown.as_str())
            {
                out.push((*t, key));
            }
        }
    };
    for krate in 0..names.crates().len() {
        for (key, _, _) in names.items(krate) {
            check(key, &mut out);
        }
        for m in names.modules(krate) {
            check(m.key, &mut out);
        }
    }
    out.as_mut_slice().sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    out.dedup();
    out
}

/// The last `Range { start: A, end: B }` of a refusal's text.
fn last_range(text: &str) -> Option<(usize, usize)> {
    let at = text.rfind("Range { start: ")?;
    let rest = &text[at + "Range { start: ".len()..];
    let comma = rest.find(',')?;
    let start = rest[..comma].trim().parse().ok()?;
    let rest = &rest[comma..];
    let end_at = rest.find("end: ")? + 5;
    let rest = &rest[end_at..];
    let close = rest.find(|c: char| !c.is_ascii_digit())?;
    Some((start, rest[..close].parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up chain in the front end's words: the innermost link leads, one a line.
    #[test]
    fn census_why_chain_is_innermost_first() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let text = "ConstPath(\"<P>::LIMBS: constant ItemKey(aaaaaaaaaaaa): a body it needs: checking ItemKey(bbbbbbbbbbbb): the constant ItemKey(cccccccccccc) (the anonymous constant `const _: usize = 4`) an array length names: Nested { item: ItemKey(dddddddddddd), shown: \"N\" }\", Range { start: 68, end: 94 })";
        let links = chain_links(text, alloc);
        assert_eq!(links.len(), 3);
        assert!(links[0].starts_with("Nested { item: ItemKey(dddddddddddd)"), "{:?}", links[0]);
        assert!(links[1].starts_with("checking ItemKey(bbbbbbbbbbbb)"), "{:?}", links[1]);
        assert!(links[2].starts_with("ConstPath(\"<P>::LIMBS"), "{:?}", links[2]);
        let keys = key_tokens(text, alloc);
        assert_eq!(keys.as_slice(), &["ItemKey(aaaaaaaaaaaa)", "ItemKey(bbbbbbbbbbbb)", "ItemKey(cccccccccccc)", "ItemKey(dddddddddddd)"]);
        assert_eq!(last_range(text), Some((68, 94)));
        assert_eq!(chain_links("one refusal", alloc).as_slice(), &["one refusal"]);
    }

    /// A block keeps [`MOST_LINES`] lines and counts the rest; a line is flat and cut.
    #[test]
    fn census_why_block_is_bounded() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut block = WhyBlock::new_in(alloc);
        for i in 0..(MOST_LINES + 3) {
            block.line(format_args!("link {i}\nsecond half"));
        }
        assert_eq!(block.lines().len(), MOST_LINES);
        assert_eq!(block.more(), 3);
        assert_eq!(block.lines()[0].as_str(), "link 0 second half");
        let mut out = AString::new_in(alloc);
        block.write_to(&mut out, "    ").expect("a string takes every line");
        assert!(out.as_str().starts_with("    WHY\n      link 0 second half\n"));
        assert!(out.as_str().ends_with("      ... 3 more\n"));
        let long: AString<'_> = flat_cut(&"x".repeat(MOST_CHARS + 10), MOST_CHARS, alloc);
        assert_eq!(long.len(), MOST_CHARS + 3);
    }

    /// The header's words: the path as written, its plain segments and the failing one.
    #[test]
    fn census_why_header_parts() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let why = "item ImplKey: Resolve(Path(PathError { module: ItemKey(0123456789ab), at: 1, why: NotFound }), Range { start: 5, end: 13 }); the header's path as written: `objc2::NSString<'a, T>`";
        let written = written_path(why).expect("the written path");
        assert_eq!(written, "objc2::NSString<'a, T>");
        assert_eq!(plain_segments(written, alloc).as_slice(), &["objc2", "NSString"]);
        assert_eq!(failed_segment(why), Some(1));
        assert_eq!(plain_segments("AccountHandle<'_, '_>", alloc).as_slice(), &["AccountHandle"]);
        assert!(declares_named("#[derive(Debug)] pub struct NSString { x: u8 }", "NSString"));
        assert!(!declares_named("pub struct NSStringRef;", "NSString"));
        let (n, line) = marked_line("fn eq(&self) -> bool {\n}", 16, 20, alloc).expect("in the text");
        assert_eq!((n, line.as_str()), (1, "fn eq(&self) -> [[bool]] {"));
    }
}
