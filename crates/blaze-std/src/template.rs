//! Bounded, 100% iterative `#![no_std]` Mustache-style template engine over [`blaze_data::Value`].
//!
//! - Compiles templates into a flat bytecode arena ([`Template`]) with `O(1)` section jump
//!   offsets and zero recursive function calls anywhere in compilation, rendering, or lookup.
//! - Supports HTML-escaped variables (`{{path}}`, `{{.}}`), raw unescaped variables
//!   (`{{{path}}}`, `{{&path}}`), sections (`{{#key}}...{{/key}}`), inverted sections
//!   (`{{^key}}...{{/key}}`), comments (`{{! ... }}`), dotted path lookup through maps and
//!   numeric list indices, and lexical context stack fallback.
//! - Enforces [`TemplateLimits`] (`most_template_bytes`, `most_nodes`, `most_depth`,
//!   `most_output_bytes`, `most_steps`) so untrusted templates cannot cause unbounded CPU,
//!   memory, or stack usage.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_data::Value;
use blaze_memchr::memmem;
use blaze_own::{AllocFailure, String, Vec};

/// Bounds enforced while compiling and rendering untrusted templates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TemplateLimits {
    /// Maximum template source length in bytes.
    pub most_template_bytes: usize,
    /// Maximum number of compiled bytecode nodes.
    pub most_nodes: usize,
    /// Maximum section nesting depth (enforced at both compile time and render time).
    pub most_depth: usize,
    /// Maximum output bytes emitted during a single render.
    pub most_output_bytes: usize,
    /// Maximum VM instruction steps executed during a single render.
    pub most_steps: usize,
}

impl TemplateLimits {
    /// Generous default bounds (1 MiB template, 16,384 nodes, depth 32, 4 MiB output, 250,000 steps).
    pub const USUAL: Self = Self {
        most_template_bytes: 1 << 20,
        most_nodes: 16_384,
        most_depth: 32,
        most_output_bytes: 4 << 20,
        most_steps: 250_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Why compiling or rendering a template failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TemplateError {
    /// Template source length exceeded [`TemplateLimits::most_template_bytes`].
    TemplateTooLarge { bytes: usize, limit: usize },
    /// Compiled bytecode node count exceeded [`TemplateLimits::most_nodes`].
    TooManyNodes { limit: usize },
    /// Section nesting depth exceeded [`TemplateLimits::most_depth`].
    TooDeep { limit: usize },
    /// Rendered output exceeded [`TemplateLimits::most_output_bytes`].
    OutputTooLarge { limit: usize },
    /// VM step count exceeded [`TemplateLimits::most_steps`].
    StepLimitExceeded { limit: usize },
    /// An opening `{{` or `{{{` tag was not closed before end of input.
    UnclosedTag { offset: usize },
    /// A tag contained no expression (such as `{{}}`, `{{#}}`, or `{{/}}`).
    EmptyTag { offset: usize },
    /// A closing section tag `{{/name}}` appeared without a matching open section.
    UnexpectedClose { offset: usize },
    /// A closing section tag `{{/other}}` did not match the innermost open section.
    MismatchedSection { offset: usize },
    /// One or more opened sections were never closed before end of input.
    UnclosedSection { offset: usize },
    /// The underlying [`fmt::Write`] output sink returned an error.
    Fmt,
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TemplateError::TemplateTooLarge { bytes, limit } => {
                write!(f, "bstd::template: template size {bytes} bytes exceeds limit {limit}")
            }
            TemplateError::TooManyNodes { limit } => {
                write!(f, "bstd::template: compiled node count exceeds limit {limit}")
            }
            TemplateError::TooDeep { limit } => {
                write!(f, "bstd::template: section nesting depth exceeds limit {limit}")
            }
            TemplateError::OutputTooLarge { limit } => {
                write!(f, "bstd::template: rendered output exceeds limit of {limit} bytes")
            }
            TemplateError::StepLimitExceeded { limit } => {
                write!(f, "bstd::template: render execution exceeded step limit of {limit}")
            }
            TemplateError::UnclosedTag { offset } => {
                write!(f, "bstd::template: unclosed tag starting at byte {offset}")
            }
            TemplateError::EmptyTag { offset } => {
                write!(f, "bstd::template: empty tag expression at byte {offset}")
            }
            TemplateError::UnexpectedClose { offset } => {
                write!(f, "bstd::template: unexpected closing section tag at byte {offset}")
            }
            TemplateError::MismatchedSection { offset } => {
                write!(f, "bstd::template: mismatched closing section tag at byte {offset}")
            }
            TemplateError::UnclosedSection { offset } => {
                write!(f, "bstd::template: unclosed section opened at byte {offset}")
            }
            TemplateError::Fmt => f.write_str("bstd::template: output writer returned fmt::Error"),
            TemplateError::Alloc(err) => write!(f, "bstd::template: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for TemplateError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Node<'a> {
    Literal(&'a str),
    EscapedVar(&'a str),
    RawVar(&'a str),
    SectionStart {
        key: &'a str,
        inverted: bool,
        end_idx: usize,
    },
    SectionEnd,
}

#[derive(Clone, Copy)]
struct OpenSection<'a> {
    key: &'a str,
    start_idx: usize,
    offset: usize,
}

enum Frame<'v, A: Allocator> {
    Inverted,
    Single,
    List {
        items: &'v [Value<A>],
        next_index: usize,
        body_start_pc: usize,
    },
}

/// A compiled Mustache-style template backed by a flat bytecode arena in `alloc`.
pub struct Template<'a, A: Allocator> {
    nodes: Vec<Node<'a>, A>,
}

impl<A: Allocator> fmt::Debug for Template<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template")
            .field("nodes", &self.nodes.as_slice())
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Template<'_, B>> for Template<'_, A> {
    fn eq(&self, other: &Template<'_, B>) -> bool {
        self.nodes.as_slice() == other.nodes.as_slice()
    }
}

impl<A: Allocator> Eq for Template<'_, A> {}

impl<A: Allocator> Template<'_, A> {
    /// Number of compiled bytecode nodes in this template.
    #[inline]
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

fn push_node<'a, A: Allocator>(
    nodes: &mut Vec<Node<'a>, A>,
    node: Node<'a>,
    limit: usize,
) -> Result<(), TemplateError> {
    if nodes.len() >= limit {
        return Err(TemplateError::TooManyNodes { limit });
    }
    nodes.try_push(node).map_err(|r| TemplateError::Alloc(r.why))
}

impl<'a, A: Allocator + Copy> Template<'a, A> {
    /// Compile Mustache template source `src` into a flat bytecode arena in `alloc`.
    pub fn compile_in(
        src: &'a str,
        limits: TemplateLimits,
        alloc: A,
    ) -> Result<Template<'a, A>, TemplateError> {
        if src.len() > limits.most_template_bytes {
            return Err(TemplateError::TemplateTooLarge {
                bytes: src.len(),
                limit: limits.most_template_bytes,
            });
        }

        let mut nodes: Vec<Node<'a>, A> = Vec::new_in(alloc);
        let mut open_sections: Vec<OpenSection<'a>, A> = Vec::new_in(alloc);
        let mut pos = 0usize;

        while pos < src.len() {
            let Some(rest) = src.get(pos..) else {
                break;
            };
            let Some(rel_open) = memmem::find(rest.as_bytes(), b"{{") else {
                if !rest.is_empty() {
                    push_node(&mut nodes, Node::Literal(rest), limits.most_nodes)?;
                }
                break;
            };

            let tag_start = pos + rel_open;
            if tag_start > pos
                && let Some(lit) = src.get(pos..tag_start)
            {
                push_node(&mut nodes, Node::Literal(lit), limits.most_nodes)?;
            }

            let after_open = src.get(tag_start..).unwrap_or("");
            if after_open.starts_with("{{{") {
                let inner_start = tag_start + 3;
                let Some(tail) = src.get(inner_start..) else {
                    return Err(TemplateError::UnclosedTag { offset: tag_start });
                };
                let Some(rel_close) = memmem::find(tail.as_bytes(), b"}}}") else {
                    return Err(TemplateError::UnclosedTag { offset: tag_start });
                };
                let key = tail.get(0..rel_close).unwrap_or("").trim();
                if key.is_empty() {
                    return Err(TemplateError::EmptyTag { offset: tag_start });
                }
                push_node(&mut nodes, Node::RawVar(key), limits.most_nodes)?;
                pos = inner_start + rel_close + 3;
                continue;
            }

            let inner_start = tag_start + 2;
            let Some(tail) = src.get(inner_start..) else {
                return Err(TemplateError::UnclosedTag { offset: tag_start });
            };
            let Some(rel_close) = memmem::find(tail.as_bytes(), b"}}") else {
                return Err(TemplateError::UnclosedTag { offset: tag_start });
            };
            let raw_inner = tail.get(0..rel_close).unwrap_or("");
            pos = inner_start + rel_close + 2;

            let trimmed = raw_inner.trim();
            let Some(&sigil) = trimmed.as_bytes().first() else {
                return Err(TemplateError::EmptyTag { offset: tag_start });
            };

            match sigil {
                b'!' => {
                    // Comments `{{! ... }}` are stripped during compilation.
                }
                b'#' | b'^' => {
                    let inverted = sigil == b'^';
                    let key = trimmed.get(1..).unwrap_or("").trim();
                    if key.is_empty() {
                        return Err(TemplateError::EmptyTag { offset: tag_start });
                    }
                    if open_sections.len() >= limits.most_depth {
                        return Err(TemplateError::TooDeep { limit: limits.most_depth });
                    }
                    let start_idx = nodes.len();
                    push_node(
                        &mut nodes,
                        Node::SectionStart { key, inverted, end_idx: 0 },
                        limits.most_nodes,
                    )?;
                    open_sections
                        .try_push(OpenSection { key, start_idx, offset: tag_start })
                        .map_err(|r| TemplateError::Alloc(r.why))?;
                }
                b'/' => {
                    let key = trimmed.get(1..).unwrap_or("").trim();
                    if key.is_empty() {
                        return Err(TemplateError::EmptyTag { offset: tag_start });
                    }
                    let Some(open) = open_sections.pop() else {
                        return Err(TemplateError::UnexpectedClose { offset: tag_start });
                    };
                    if open.key != key {
                        return Err(TemplateError::MismatchedSection { offset: tag_start });
                    }
                    let end_idx = nodes.len();
                    push_node(&mut nodes, Node::SectionEnd, limits.most_nodes)?;
                    if let Some(start_node) = nodes.get_mut(open.start_idx) {
                        match start_node {
                            Node::SectionStart { end_idx: target, .. } => {
                                *target = end_idx;
                            }
                            Node::Literal(_)
                            | Node::EscapedVar(_)
                            | Node::RawVar(_)
                            | Node::SectionEnd => {}
                        }
                    }
                }
                b'&' => {
                    let key = trimmed.get(1..).unwrap_or("").trim();
                    if key.is_empty() {
                        return Err(TemplateError::EmptyTag { offset: tag_start });
                    }
                    push_node(&mut nodes, Node::RawVar(key), limits.most_nodes)?;
                }
                _ => {
                    push_node(&mut nodes, Node::EscapedVar(trimmed), limits.most_nodes)?;
                }
            }
        }

        if let Some(unclosed) = open_sections.last() {
            return Err(TemplateError::UnclosedSection { offset: unclosed.offset });
        }

        Ok(Template { nodes })
    }

    /// Render this compiled template against `data` into `out`, using `alloc` for explicit VM stacks.
    pub fn render_into<W: fmt::Write>(
        &self,
        data: &Value<A>,
        limits: TemplateLimits,
        out: &mut W,
        alloc: A,
    ) -> Result<(), TemplateError> {
        self.render_into_planted(data, limits, out, alloc, false)
    }

    fn render_into_planted<W: fmt::Write>(
        &self,
        data: &Value<A>,
        limits: TemplateLimits,
        out: &mut W,
        alloc: A,
        plant_skip_html_escape: bool,
    ) -> Result<(), TemplateError> {
        let mut contexts: Vec<&Value<A>, A> = Vec::new_in(alloc);
        contexts.try_push(data).map_err(|r| TemplateError::Alloc(r.why))?;
        let mut frames: Vec<Frame<'_, A>, A> = Vec::new_in(alloc);

        let mut bw = BoundedWriter {
            inner: out,
            written: 0,
            limit: limits.most_output_bytes,
            err: None,
        };

        let mut pc = 0usize;
        let mut steps = 0usize;

        while let Some(node) = self.nodes.get(pc) {
            steps += 1;
            if steps > limits.most_steps {
                return Err(TemplateError::StepLimitExceeded { limit: limits.most_steps });
            }

            match *node {
                Node::Literal(lit) => {
                    bw.write_str_checked(lit)?;
                    pc += 1;
                }
                Node::EscapedVar(key) => {
                    if let Some(val) = resolve_path(contexts.as_slice(), key) {
                        render_value(val, !plant_skip_html_escape, &mut bw)?;
                    }
                    pc += 1;
                }
                Node::RawVar(key) => {
                    if let Some(val) = resolve_path(contexts.as_slice(), key) {
                        render_value(val, false, &mut bw)?;
                    }
                    pc += 1;
                }
                Node::SectionStart { key, inverted, end_idx } => {
                    let resolved = resolve_path(contexts.as_slice(), key);
                    if inverted {
                        let truthy = resolved.is_some_and(is_truthy);
                        if truthy {
                            pc = end_idx + 1;
                        } else {
                            if frames.len() >= limits.most_depth {
                                return Err(TemplateError::TooDeep { limit: limits.most_depth });
                            }
                            frames
                                .try_push(Frame::Inverted)
                                .map_err(|r| TemplateError::Alloc(r.why))?;
                            pc += 1;
                        }
                    } else {
                        let Some(val) = resolved else {
                            pc = end_idx + 1;
                            continue;
                        };
                        if !is_truthy(val) {
                            pc = end_idx + 1;
                            continue;
                        }
                        match val {
                            Value::List(items) => {
                                if let Some(first) = items.first() {
                                    if frames.len() >= limits.most_depth {
                                        return Err(TemplateError::TooDeep {
                                            limit: limits.most_depth,
                                        });
                                    }
                                    contexts
                                        .try_push(first)
                                        .map_err(|r| TemplateError::Alloc(r.why))?;
                                    frames
                                        .try_push(Frame::List {
                                            items: items.as_slice(),
                                            next_index: 1,
                                            body_start_pc: pc + 1,
                                        })
                                        .map_err(|r| TemplateError::Alloc(r.why))?;
                                    pc += 1;
                                } else {
                                    pc = end_idx + 1;
                                }
                            }
                            Value::Null
                            | Value::Bool(_)
                            | Value::Int(_)
                            | Value::Float(_)
                            | Value::Text(_)
                            | Value::Bytes(_)
                            | Value::Map(_) => {
                                if frames.len() >= limits.most_depth {
                                    return Err(TemplateError::TooDeep {
                                        limit: limits.most_depth,
                                    });
                                }
                                contexts
                                    .try_push(val)
                                    .map_err(|r| TemplateError::Alloc(r.why))?;
                                frames
                                    .try_push(Frame::Single)
                                    .map_err(|r| TemplateError::Alloc(r.why))?;
                                pc += 1;
                            }
                        }
                    }
                }
                Node::SectionEnd => match frames.last_mut() {
                    None => {
                        pc += 1;
                    }
                    Some(Frame::Inverted) => {
                        let _ = frames.pop();
                        pc += 1;
                    }
                    Some(Frame::Single) => {
                        let _ = frames.pop();
                        let _ = contexts.pop();
                        pc += 1;
                    }
                    Some(Frame::List { items, next_index, body_start_pc }) => {
                        if let Some(next_item) = items.get(*next_index) {
                            *next_index += 1;
                            if let Some(top) = contexts.last_mut() {
                                *top = next_item;
                            }
                            pc = *body_start_pc;
                        } else {
                            let _ = frames.pop();
                            let _ = contexts.pop();
                            pc += 1;
                        }
                    }
                },
            }
        }

        Ok(())
    }

    /// Render this compiled template against `data` into a new [`String`] in `alloc`.
    pub fn render_in(
        &self,
        data: &Value<A>,
        limits: TemplateLimits,
        alloc: A,
    ) -> Result<String<A>, TemplateError> {
        self.render_in_planted(data, limits, alloc, false)
    }

    fn render_in_planted(
        &self,
        data: &Value<A>,
        limits: TemplateLimits,
        alloc: A,
        plant_skip_html_escape: bool,
    ) -> Result<String<A>, TemplateError> {
        let mut out = String::new_in(alloc);
        let mut sink = TryStringSink { buf: &mut out, alloc_err: None };
        if let Err(err) =
            self.render_into_planted(data, limits, &mut sink, alloc, plant_skip_html_escape)
        {
            if err == TemplateError::Fmt
                && let Some(alloc_err) = sink.alloc_err
            {
                return Err(TemplateError::Alloc(alloc_err));
            }
            return Err(err);
        }
        Ok(out)
    }
}

/// Compile and render Mustache template `src` against `data` in a single call.
pub fn render_in<A: Allocator + Copy>(
    src: &str,
    data: &Value<A>,
    limits: TemplateLimits,
    alloc: A,
) -> Result<String<A>, TemplateError> {
    let tpl = Template::compile_in(src, limits, alloc)?;
    tpl.render_in(data, limits, alloc)
}

fn is_truthy<A: Allocator>(val: &Value<A>) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(n) => n.as_i128() != 0,
        Value::Float(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(s) => !s.is_empty(),
        Value::Bytes(b) => !b.is_empty(),
        Value::List(list) => !list.is_empty(),
        Value::Map(_) => true,
    }
}

fn lookup_step<'v, A: Allocator>(val: &'v Value<A>, seg: &str) -> Option<&'v Value<A>> {
    match val {
        Value::Map(map) => map.get(seg),
        Value::List(list) => {
            let idx = seg.parse::<usize>().ok()?;
            list.get(idx)
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_) => None,
    }
}

fn resolve_path<'v, A: Allocator>(stack: &[&'v Value<A>], key: &str) -> Option<&'v Value<A>> {
    if key == "." {
        return stack.last().copied();
    }

    let (first_seg, rest) = match key.split_once('.') {
        Some((head, tail)) => (head, Some(tail)),
        None => (key, None),
    };

    if first_seg.is_empty() {
        return None;
    }

    for &ctx in stack.iter().rev() {
        if let Some(found) = lookup_step(ctx, first_seg) {
            let mut cur = found;
            if let Some(tail) = rest {
                for seg in tail.split('.') {
                    if seg.is_empty() {
                        return None;
                    }
                    match lookup_step(cur, seg) {
                        Some(next) => cur = next,
                        None => return None,
                    }
                }
            }
            return Some(cur);
        }
    }

    None
}

struct BoundedWriter<'a, W: fmt::Write> {
    inner: &'a mut W,
    written: usize,
    limit: usize,
    err: Option<TemplateError>,
}

impl<W: fmt::Write> BoundedWriter<'_, W> {
    fn write_str_checked(&mut self, s: &str) -> Result<(), TemplateError> {
        if let Some(err) = self.err {
            return Err(err);
        }
        let next = self.written.saturating_add(s.len());
        if next > self.limit {
            let err = TemplateError::OutputTooLarge { limit: self.limit };
            self.err = Some(err);
            return Err(err);
        }
        match self.inner.write_str(s) {
            Ok(()) => {
                self.written = next;
                Ok(())
            }
            Err(_) => {
                let err = TemplateError::Fmt;
                self.err = Some(err);
                Err(err)
            }
        }
    }
}

impl<W: fmt::Write> fmt::Write for BoundedWriter<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str_checked(s).map_err(|_| fmt::Error)
    }
}

struct TryStringSink<'a, A: Allocator> {
    buf: &'a mut String<A>,
    alloc_err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringSink<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.alloc_err.is_some() {
            return Err(fmt::Error);
        }
        match self.buf.try_push_str(s) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.alloc_err = Some(err);
                Err(fmt::Error)
            }
        }
    }
}

fn write_fmt_checked<W: fmt::Write>(
    out: &mut BoundedWriter<'_, W>,
    args: fmt::Arguments<'_>,
) -> Result<(), TemplateError> {
    match fmt::Write::write_fmt(out, args) {
        Ok(()) => Ok(()),
        Err(_) => match out.err {
            Some(err) => Err(err),
            None => Err(TemplateError::Fmt),
        },
    }
}

fn write_html_escaped<W: fmt::Write>(
    text: &str,
    out: &mut BoundedWriter<'_, W>,
) -> Result<(), TemplateError> {
    let bytes = text.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        let entity = match b {
            b'&' => Some("&amp;"),
            b'<' => Some("&lt;"),
            b'>' => Some("&gt;"),
            b'"' => Some("&quot;"),
            b'\'' => Some("&#39;"),
            _ => None,
        };
        if let Some(esc) = entity {
            if i > seg_start
                && let Some(chunk) = text.get(seg_start..i)
            {
                out.write_str_checked(chunk)?;
            }
            out.write_str_checked(esc)?;
            i += 1;
            seg_start = i;
            continue;
        }
        i += 1;
    }
    if i > seg_start
        && let Some(chunk) = text.get(seg_start..i)
    {
        out.write_str_checked(chunk)?;
    }
    Ok(())
}

fn render_value<W: fmt::Write, A: Allocator>(
    val: &Value<A>,
    escape_html: bool,
    out: &mut BoundedWriter<'_, W>,
) -> Result<(), TemplateError> {
    match val {
        Value::Null | Value::Bytes(_) | Value::List(_) | Value::Map(_) => Ok(()),
        Value::Bool(b) => out.write_str_checked(if *b { "true" } else { "false" }),
        Value::Int(n) => write_fmt_checked(out, format_args!("{n}")),
        Value::Float(f) => write_fmt_checked(out, format_args!("{f}")),
        Value::Text(s) => {
            if escape_html {
                write_html_escaped(s.as_str(), out)
            } else {
                out.write_str_checked(s.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_data::{Int, Map, Value};
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::{Template, TemplateError, TemplateLimits, render_in};

    fn text_val<A: Allocator>(s: &str, alloc: A) -> Value<A> {
        Value::Text(OwnString::from_str_in(s, alloc))
    }

    fn insert_kv<A: Allocator + Copy>(map: &mut Map<A>, key: &str, val: Value<A>, alloc: A) {
        map.insert(OwnString::from_str_in(key, alloc), val);
    }

    #[test]
    fn template_variables_and_html_escaping() {
        let h = heap();
        let alloc = h.alloc();

        let mut root = Map::new_in(alloc);
        insert_kv(
            &mut root,
            "html",
            text_val("<script>alert(\"x\" & 'y')</script>", alloc),
            alloc,
        );
        insert_kv(&mut root, "count", Value::Int(Int::from_i64(-42)), alloc);
        insert_kv(&mut root, "pi", Value::Float(3.5), alloc);
        insert_kv(&mut root, "flag", Value::Bool(true), alloc);
        insert_kv(&mut root, "nothing", Value::Null, alloc);
        let data = Value::Map(root);

        let src = "esc={{html}}|raw1={{{html}}}|raw2={{&html}}|n={{count}}|f={{pi}}|b={{flag}}|z={{nothing}}|m={{missing}}";
        let rendered = render_in(src, &data, TemplateLimits::SHIPPED, alloc).expect("render");
        assert_eq!(
            rendered.as_str(),
            concat!(
                "esc=&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;",
                "|raw1=<script>alert(\"x\" & 'y')</script>",
                "|raw2=<script>alert(\"x\" & 'y')</script>",
                "|n=-42|f=3.5|b=true|z=|m="
            )
        );
    }

    #[test]
    fn template_sections_lists_inverted_comments_and_paths() {
        let h = heap();
        let alloc = h.alloc();

        let mut profile = Map::new_in(alloc);
        insert_kv(&mut profile, "name", text_val("Ada", alloc), alloc);

        let mut user = Map::new_in(alloc);
        insert_kv(&mut user, "profile", Value::Map(profile), alloc);
        insert_kv(&mut user, "role", text_val("admin", alloc), alloc);

        let mut item0 = Map::new_in(alloc);
        insert_kv(&mut item0, "title", text_val("First", alloc), alloc);
        let mut item1 = Map::new_in(alloc);
        insert_kv(&mut item1, "title", text_val("Second", alloc), alloc);
        let mut items = OwnVec::new_in(alloc);
        items.push(Value::Map(item0));
        items.push(Value::Map(item1));

        let mut tags = OwnVec::new_in(alloc);
        tags.push(text_val("rust", alloc));
        tags.push(text_val("blaze", alloc));

        let mut root = Map::new_in(alloc);
        insert_kv(&mut root, "site", text_val("Blaze", alloc), alloc);
        insert_kv(&mut root, "user", Value::Map(user), alloc);
        insert_kv(&mut root, "items", Value::List(items), alloc);
        insert_kv(&mut root, "tags", Value::List(tags), alloc);
        insert_kv(&mut root, "empty_list", Value::List(OwnVec::new_in(alloc)), alloc);
        insert_kv(&mut root, "empty_text", text_val("", alloc), alloc);
        insert_kv(&mut root, "zero", Value::Int(Int::from_u64(0)), alloc);
        insert_kv(&mut root, "off", Value::Bool(false), alloc);
        insert_kv(&mut root, "nil", Value::Null, alloc);
        insert_kv(&mut root, "empty_map", Value::Map(Map::new_in(alloc)), alloc);
        let data = Value::Map(root);

        let src = concat!(
            "{{! header comment }}",
            "Hi {{user.profile.name}} ({{items.1.title}})!",
            "{{#user}} [{{site}}:{{role}}]{{/user}}",
            " tags=[{{#tags}}<{{.}}>{{/tags}}]",
            " items=[{{#items}}{{title}}@{{site}};{{/items}}]",
            "{{#empty_list}}NO{{/empty_list}}{{^empty_list}}E1{{/empty_list}}",
            "{{#empty_text}}NO{{/empty_text}}{{^empty_text}}E2{{/empty_text}}",
            "{{#zero}}NO{{/zero}}{{^zero}}E3{{/zero}}",
            "{{#off}}NO{{/off}}{{^off}}E4{{/off}}",
            "{{#nil}}NO{{/nil}}{{^nil}}E5{{/nil}}",
            "{{#empty_map}}M1{{/empty_map}}{{^empty_map}}NO{{/empty_map}}",
        );

        let rendered = render_in(src, &data, TemplateLimits::SHIPPED, alloc).expect("render");
        assert_eq!(
            rendered.as_str(),
            "Hi Ada (Second)! [Blaze:admin] tags=[<rust><blaze>] items=[First@Blaze;Second@Blaze;]E1E2E3E4E5M1"
        );
    }

    #[test]
    fn template_errors_and_limits() {
        let h = heap();
        let alloc = h.alloc();

        assert_eq!(
            Template::compile_in("{{unclosed", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::UnclosedTag { offset: 0 })
        );
        assert_eq!(
            Template::compile_in("{{{unclosed}}", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::UnclosedTag { offset: 0 })
        );
        assert_eq!(
            Template::compile_in("{{   }}", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::EmptyTag { offset: 0 })
        );
        assert_eq!(
            Template::compile_in("{{# }}x{{/a}}", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::EmptyTag { offset: 0 })
        );
        assert_eq!(
            Template::compile_in("{{/orphan}}", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::UnexpectedClose { offset: 0 })
        );
        assert_eq!(
            Template::compile_in("{{#a}}{{/b}}", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::MismatchedSection { offset: 6 })
        );
        assert_eq!(
            Template::compile_in("{{#a}}hello", TemplateLimits::SHIPPED, alloc),
            Err(TemplateError::UnclosedSection { offset: 0 })
        );

        // Compile-time depth limit.
        assert_eq!(
            Template::compile_in(
                "{{#a}}{{#b}}{{/b}}{{/a}}",
                TemplateLimits { most_depth: 1, ..TemplateLimits::SHIPPED },
                alloc,
            ),
            Err(TemplateError::TooDeep { limit: 1 })
        );

        // Node limit.
        assert_eq!(
            Template::compile_in(
                "a{{x}}b",
                TemplateLimits { most_nodes: 2, ..TemplateLimits::SHIPPED },
                alloc,
            ),
            Err(TemplateError::TooManyNodes { limit: 2 })
        );

        // Output byte limit.
        let mut root = Map::new_in(alloc);
        insert_kv(&mut root, "s", text_val("0123456789", alloc), alloc);
        let data = Value::Map(root);
        assert_eq!(
            render_in(
                "{{s}}",
                &data,
                TemplateLimits { most_output_bytes: 5, ..TemplateLimits::SHIPPED },
                alloc,
            ),
            Err(TemplateError::OutputTooLarge { limit: 5 })
        );

        // Step limit on list iteration.
        let mut big = OwnVec::new_in(alloc);
        for _ in 0..20 {
            big.push(Value::Int(Int::from_u64(1)));
        }
        let mut step_root = Map::new_in(alloc);
        insert_kv(&mut step_root, "xs", Value::List(big), alloc);
        let step_data = Value::Map(step_root);
        assert_eq!(
            render_in(
                "{{#xs}}{{.}}{{/xs}}",
                &step_data,
                TemplateLimits { most_steps: 10, ..TemplateLimits::SHIPPED },
                alloc,
            ),
            Err(TemplateError::StepLimitExceeded { limit: 10 })
        );
    }

    fn draw_xss_payload<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const CHUNKS: &[&str] = &[
            "plain",
            "<script>",
            "</script>",
            "<img src=x onerror=\"alert(1)\">",
            "'single'",
            "\"double\"",
            "&amp;",
            "<",
            ">",
            "café",
        ];
        let count = tape.int_in(1usize..=6, "xss_chunks");
        let mut out = OwnString::new_in(alloc);
        for _ in 0..count {
            if let Some(&chunk) = tape.pick(CHUNKS, "xss_chunk") {
                out.push_str(chunk);
            }
        }
        out
    }

    fn check_xss_safety<A: Allocator + Copy>(
        payload: &str,
        plant_skip_html_escape: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut root = Map::new_in(alloc);
        insert_kv(&mut root, "x", text_val(payload, alloc), alloc);
        let data = Value::Map(root);
        let tpl = Template::compile_in("{{x}}", TemplateLimits::SHIPPED, alloc)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "compile failed: {e}").as_str().into()))?;
        let rendered = tpl
            .render_in_planted(&data, TemplateLimits::SHIPPED, alloc, plant_skip_html_escape)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "render failed: {e}").as_str().into()))?;

        let s = rendered.as_str();
        if s.contains('<') || s.contains('>') || s.contains('"') || s.contains('\'') {
            return Err(Broke::Oracle(
                format_in!(alloc, "unescaped HTML character leaked into output: {s:?}")
                    .as_str()
                    .into(),
            ));
        }
        if s.contains("<script") {
            return Err(Broke::Oracle(
                format_in!(alloc, "unescaped <script leaked: {s:?}").as_str().into(),
            ));
        }
        Ok(())
    }

    /// The invariant: escaped template interpolation `{{var}}` never emits unescaped `<script` into rendered output.
    #[test]
    fn property_xss_safety_escaped_var() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let payload = draw_xss_payload(tape, h.alloc());
            check_xss_safety(payload.as_str(), false, h.alloc())
        });
    }

    #[test]
    fn property_hostile_template_never_panics() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            const TOKENS: &[&str] = &[
                "{{", "}}", "{{{", "}}}", "#", "^", "/", "!", "&", ".", "a", "b", " ", "\n", "{",
                "}", "a.b", "0",
            ];
            let len = tape.int_in(0usize..=32, "token_count");
            let mut src = OwnString::new_in(alloc);
            for _ in 0..len {
                if let Some(&tok) = tape.pick(TOKENS, "tok") {
                    src.push_str(tok);
                }
            }

            let mut inner = OwnVec::new_in(alloc);
            inner.push(text_val("v1", alloc));
            inner.push(text_val("<v2>", alloc));
            let mut root = Map::new_in(alloc);
            insert_kv(&mut root, "a", Value::List(inner), alloc);
            insert_kv(&mut root, "b", text_val("hello", alloc), alloc);
            let data = Value::Map(root);

            let tight = TemplateLimits {
                most_template_bytes: 512,
                most_nodes: 64,
                most_depth: 8,
                most_output_bytes: 256,
                most_steps: 256,
            };

            if let Ok(tpl) = Template::compile_in(src.as_str(), tight, alloc) {
                match tpl.render_in(&data, tight, alloc) {
                    Ok(rendered) => {
                        if rendered.len() > tight.most_output_bytes {
                            return Err(Broke::Oracle(
                                format_in!(alloc, "output exceeded byte limit").as_str().into(),
                            ));
                        }
                    }
                    Err(
                        TemplateError::OutputTooLarge { .. }
                        | TemplateError::StepLimitExceeded { .. }
                        | TemplateError::TooDeep { .. }
                        | TemplateError::Alloc(_),
                    ) => {}
                    Err(other) => {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "unexpected render error: {other}")
                                .as_str()
                                .into(),
                        ));
                    }
                }
            }
            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_html_escape() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let payload = draw_xss_payload(tape, h.alloc());
            check_xss_safety(payload.as_str(), plant, h.alloc())
        });
    }
}
