//! `cargo tree --format`: `src/ops/cargo_tree/format/` of cargo 1.101.0 (cad07cf8e5).

use std::fmt::Write as _;

use anyhow::{Error, bail};

use super::graph::{Graph, Node, NodeId};
use super::packages::{Pkg, Table};

#[derive(Clone, Copy)]
enum Chunk<'f> {
    Raw(&'f str),
    Package,
    License,
    Repository,
    Features,
    LibName,
}

pub(crate) struct Pattern<'f>(Vec<Chunk<'f>>);

impl<'f> Pattern<'f> {
    pub(crate) fn new(format: &'f str) -> Result<Pattern<'f>, Error> {
        // Text and arguments alternate, and each argument has its braces.
        let braces = format.bytes().filter(|&b| b == b'{' || b == b'}').count();
        let mut chunks = Vec::with_capacity(braces + 1);
        for raw in Parser::new(format) {
            let chunk = match raw {
                RawChunk::Text(text) => Chunk::Raw(text),
                RawChunk::Argument("package" | "p") => Chunk::Package,
                RawChunk::Argument("license" | "l") => Chunk::License,
                RawChunk::Argument("repository" | "r") => Chunk::Repository,
                RawChunk::Argument("features" | "f") => Chunk::Features,
                RawChunk::Argument("lib") => Chunk::LibName,
                RawChunk::Argument(a) => bail!("unsupported pattern `{a}`"),
                RawChunk::Error(err) => bail!("{err}"),
            };
            chunks.push(chunk);
        }
        Ok(Pattern(chunks))
    }

    /// Writes `node` as the pattern has it into `out`.
    pub(crate) fn write(&self, graph: &Graph<'_>, node: NodeId, labels: &mut Labels, out: &mut String) {
        let packages = graph.packages();
        match graph.node(node) {
            Node::Package { pkg, features, .. } => {
                let package = packages.info(pkg);
                for chunk in &self.0 {
                    match *chunk {
                        Chunk::Raw(s) => out.push_str(s),
                        Chunk::Package => out.push_str(labels.of(packages, pkg)),
                        Chunk::License => {
                            if let Some(license) = package.license {
                                out.push_str(license);
                            }
                        }
                        Chunk::Repository => {
                            if let Some(repository) = package.repository {
                                out.push_str(repository);
                            }
                        }
                        Chunk::Features => {
                            for (i, feature) in graph.features(features).iter().enumerate() {
                                if i != 0 {
                                    out.push(',');
                                }
                                out.push_str(feature);
                            }
                        }
                        Chunk::LibName => {
                            if let Some(lib) = package.lib {
                                out.push_str(&lib);
                            }
                        }
                    }
                }
            }
            Node::Feature { name, of } => match graph.node(of) {
                Node::Package { pkg, .. } => {
                    let _ = write!(out, "{} feature \"{}\"", packages.id(pkg).name(), name);
                    if graph.is_cli_feature(node) {
                        out.push_str(" (command-line)");
                    }
                }
                // The `of` of a feature node must point to a package node, see `add_feature`.
                for_node => panic!("unexpected feature node {for_node:?}"),
            },
        }
    }
}

/// Each package's `{p}`: its name, version and source, written once and copied after, since a
/// source's `Display` builds its text anew each time.
pub(crate) struct Labels {
    text: String,
    /// Of each package, where its text is in `text`; `u32::MAX` before it is written.
    at: Vec<(u32, u32)>,
}

impl Labels {
    pub(crate) fn new(packages: usize) -> Labels {
        let mut at = Vec::with_capacity(packages);
        at.resize(packages, (u32::MAX, 0));
        Labels { text: String::with_capacity(64 * packages), at }
    }

    fn of(&mut self, packages: &Table<'_>, pkg: Pkg) -> &str {
        if self.at[pkg.index()].0 == u32::MAX {
            let start = self.text.len();
            let id = packages.id(pkg);
            let proc_macro_suffix = if packages.any_proc_macro(pkg) { " (proc-macro)" } else { "" };
            let _ = write!(self.text, "{} v{}{}", id.name(), id.version(), proc_macro_suffix);
            let source_id = id.source_id();
            if !source_id.is_crates_io() {
                let _ = write!(self.text, " ({source_id})");
            }
            self.at[pkg.index()] = (start as u32, self.text.len() as u32);
        }
        let (start, end) = self.at[pkg.index()];
        &self.text[start as usize..end as usize]
    }
}

enum RawChunk<'a> {
    /// Raw text to include in the output.
    Text(&'a str),
    /// A substitution to place in the output: `p` is the package's name and version.
    Argument(&'a str),
    /// What is wrong with the format string, for its reader.
    Error(&'static str),
}

/// The `--format` string: text with alphabetic names in braces, `{p}` or `{foo}`, that are
/// substituted, and `{{` and `}}` for braces themselves.
struct Parser<'a> {
    s: &'a str,
    it: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Parser<'a> {
        Parser { s, it: s.char_indices().peekable() }
    }

    fn consume(&mut self, ch: char) -> bool {
        match self.it.peek() {
            Some(&(_, c)) if c == ch => {
                self.it.next();
                true
            }
            _ => false,
        }
    }

    fn name(&mut self) -> &'a str {
        let start = match self.it.peek() {
            Some(&(pos, ch)) if ch.is_alphabetic() => {
                self.it.next();
                pos
            }
            _ => return "",
        };
        loop {
            match self.it.peek() {
                Some(&(_, ch)) if ch.is_alphanumeric() => {
                    self.it.next();
                }
                Some(&(end, _)) => return &self.s[start..end],
                None => return &self.s[start..],
            }
        }
    }

    fn text(&mut self, start: usize) -> RawChunk<'a> {
        while let Some(&(pos, ch)) = self.it.peek() {
            match ch {
                '{' | '}' => return RawChunk::Text(&self.s[start..pos]),
                _ => {
                    self.it.next();
                }
            }
        }
        RawChunk::Text(&self.s[start..])
    }
}

impl<'a> Iterator for Parser<'a> {
    type Item = RawChunk<'a>;

    fn next(&mut self) -> Option<RawChunk<'a>> {
        match self.it.peek() {
            Some(&(_, '{')) => {
                self.it.next();
                if self.consume('{') {
                    Some(RawChunk::Text("{"))
                } else {
                    let chunk = RawChunk::Argument(self.name());
                    if self.consume('}') {
                        Some(chunk)
                    } else {
                        for _ in &mut self.it {}
                        Some(RawChunk::Error("expected '}'"))
                    }
                }
            }
            Some(&(_, '}')) => {
                self.it.next();
                if self.consume('}') {
                    Some(RawChunk::Text("}"))
                } else {
                    Some(RawChunk::Error("unexpected '}'"))
                }
            }
            Some(&(i, _)) => Some(self.text(i)),
            None => None,
        }
    }
}
