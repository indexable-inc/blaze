//! Text-based stubs (`.tbd`): the YAML Apple's `tapi` writes for the SDK's libraries, read
//! by hand for the subset it uses (versions 1 to 4; version 5 is JSON).
//!
//! The target is chosen per document as ld64's `tapi` reader does: the output's arch when
//! the document lists it on the output's platform, else the ABI-compatible one it lists
//! there (`arm64e` for `arm64`, `x86_64h` for `x86_64`), since an `arm64` process loads the
//! system's `arm64e` libraries (the SDK's `libSystem.B.tbd` lists no `arm64-macos` itself).
//!
//! The YAML is lexed over the bytes (docs/v2/plan/lanes/link/speed-link-edge-macho-dylib.md):
//! one pass per line finds comments, quotes and flow brackets eight bytes a step (SWAR), and
//! one reads values onto a flat tape of 12-byte [`Node`]s whose scalars are spans of the
//! text, copied only when unescaped or wrapped across lines. A list is decoded only when its
//! targets select it, and each name is hashed once into an open-addressing set ([`Seen`])
//! and, when new, appended to the file's one [`Names`] arena, sized before the first name.

#![forbid(unsafe_code)]

use std::hash::BuildHasher as _;
use std::ops::Range;

use blaze_codegen::Arch;
use blaze_graph::par;
use blaze_hash::{FixedState, HashMap, HashSet};

use crate::model::{Names, Shared, SharedSymbol, SymbolKind};

#[cfg(any(test, feature = "fuzz"))]
mod reference;

/// A text-based stub (`.tbd`, as the macOS SDK ships `libSystem.tbd`) for `arch` on
/// `platform` (`LC_BUILD_VERSION`'s number: 1 macOS, 2 iOS, 3 tvOS, 4 watchOS, 6 Mac
/// Catalyst, 7 iOS simulator, 8 tvOS simulator, 9 watchOS simulator, 11 visionOS,
/// 12 visionOS simulator).
pub(super) fn read(name: &str, text: &str, arch: Arch, platform: u32) -> Result<Shared, String> {
    // alloc: error path
    read_documents(text, arch, platform).map_err(|what| format!("{name}: {what}"))
}

/// The largest text read: the tape's spans are `u32`, and the scratch buffer holds at most
/// one copy of the text.
const MAX_TEXT: usize = 1 << 31;

fn read_documents(text: &str, arch: Arch, platform: u32) -> Result<Shared, String> {
    if matches!(arch, Arch::Wasm32) {
        // alloc: error path
        return Err("a `.tbd` describes a Mach-O library; this output is wasm32".to_owned());
    }
    if text.trim_start().starts_with('{') {
        // alloc: error path
        return Err("a JSON `.tbd` (version 5) is not read: only versions 1 to 4".to_owned());
    }
    if text.len() >= MAX_TEXT {
        // alloc: error path
        return Err(format!("{} bytes: a `.tbd` is read up to 2 GiB", text.len()));
    }
    let raw = documents(text)?;
    if raw.is_empty() {
        // alloc: error path
        return Err("no document".to_owned());
    }
    let (tape, docs, targets) = parse_all(text, &raw, raw.len() > 1 && text.len() >= PARALLEL)?;

    let view = tape.view(text);
    let mut by_name = HashMap::default();
    for (at, doc) in docs.iter().enumerate() {
        by_name.entry(view.str(doc.install_name)).or_insert(at);
    }
    let stream = Stream { view, docs: &docs, targets: &targets, by_name };
    let mut plan = Plan::default();
    let mut seen = HashSet::default();
    seen.insert(view.str(docs[0].install_name));
    stream.fold(0, arch, platform, &mut seen, &mut plan)?;
    let Exports { symbols, names, .. } = decode(view, &plan)?;
    Ok(Shared {
        // alloc: `Shared::name`, once per file
        name: view.str(docs[0].install_name).to_owned(),
        symbols,
        names,
        // alloc: empty `Vec`s, which don't allocate
        versions: Vec::new(),
        // alloc: empty `Vec`s, which don't allocate
        more: Vec::new(),
        // alloc: `Shared::inputs`: the re-exports outside the file, once per file
        inputs: plan.inputs.iter().map(|it| (*it).to_owned()).collect(),
    })
}

/// Below this many bytes of text a stream's documents are parsed on the calling thread: a
/// parse of them all is then about as short as handing one to another core.
const PARALLEL: usize = 64 << 10;

/// Every document on one tape, each with its [`Doc`], and the targets they list.
///
/// In `parallel` (A6 of `v2/requests/requests-link-fuzz.md`), each document is parsed onto a
/// tape of its own on a worker (libSystem's 40-odd documents on as many cores), then the tapes
/// are joined in document order, each node's subtree end and scratch offset and each doc's
/// and target's nodes moved by where its tape lands. The tape, docs, targets and first error
/// are those of parsing the documents one after another.
fn parse_all(text: &str, raw: &[RawDoc<'_>], parallel: bool) -> Result<(Tape, Vec<Doc>, Vec<Target>), String> {
    // A document's logical lines and their pieces are at most its physical lines: sized once
    // for the longest, and reused by every document a thread parses.
    let most = raw.iter().map(|it| it.count).max().unwrap_or(0);
    if !parallel {
        // alloc: the tape, once per file; `scratch` stays empty until a scalar is unescaped or wraps
        let mut tape = Tape { nodes: Vec::with_capacity(text.len() / 16), scratch: String::new() };
        let mut lines = Vec::with_capacity(most);
        let mut pieces = Vec::with_capacity(most);
        // alloc: once per file, reused by every document
        let mut targets = Vec::new();
        let mut docs = Vec::with_capacity(raw.len());
        for doc in raw {
            let root = parse(text, doc, &mut tape, &mut lines, &mut pieces)?;
            docs.push(Doc::new(&tape.view(text), doc, root, &mut targets)?);
        }
        return Ok((tape, docs, targets));
    }
    let mut parts: Vec<Result<(Tape, Doc, Vec<Target>), String>> = Vec::with_capacity(raw.len());
    parts.clear();
    par::extend_init(
        &mut parts,
        raw,
        || (Vec::<Line>::with_capacity(most), Vec::<Piece>::with_capacity(most)),
        |(lines, pieces), one| -> Result<(Tape, Doc, Vec<Target>), String> {
            let len = one.lines.end - one.lines.start;
            // alloc: the document's own tape, once per document
            let mut tape = Tape { nodes: Vec::with_capacity(len / 16), scratch: String::new() };
            // alloc: none until the document lists a target, then once per document
            let mut targets = Vec::new();
            let root = parse(text, one, &mut tape, lines, pieces)?;
            let doc = Doc::new(&tape.view(text), one, root, &mut targets)?;
            Ok((tape, doc, targets))
        },
    );
    // alloc: the documents' parses, once per file
    let parts: Vec<(Tape, Doc, Vec<Target>)> = parts.into_iter().collect::<Result<_, _>>()?;

    // Where each document's nodes, scratch and targets land.
    let mut bases: Vec<(u32, u32, u32)> = Vec::with_capacity(parts.len());
    let (mut nodes, mut held, mut listed) = (0usize, 0usize, 0usize);
    for (part, _, targets) in &parts {
        bases.push((nodes as u32, held as u32, listed as u32));
        nodes += part.nodes.len();
        held += part.scratch.len();
        listed += targets.len();
    }
    let mut tape = Tape { nodes: Vec::with_capacity(nodes), scratch: String::with_capacity(held) };
    tape.nodes.resize(nodes, Node { kind: Kind::Text, at: 0, len: 0 });
    {
        let mut chunks: Vec<&mut [Node]> = Vec::with_capacity(parts.len());
        let mut rest = &mut tape.nodes[..];
        for (part, _, _) in &parts {
            let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(part.nodes.len());
            chunks.push(chunk);
            rest = tail;
        }
        par::for_each(par::zip(par::zip(chunks, &parts), &bases), |((chunk, part), &(base, offset, _))| {
            for (out, node) in chunk.iter_mut().zip(&part.0.nodes) {
                *out = match node.kind {
                    Kind::Seq | Kind::Map => Node { at: node.at + base, ..*node },
                    Kind::Scratch => Node { at: node.at + offset, ..*node },
                    Kind::Text => *node,
                };
            }
        });
    }
    let mut targets = Vec::with_capacity(listed);
    let mut docs = Vec::with_capacity(parts.len());
    for ((part, doc, its), (base, _, first)) in parts.iter().zip(bases) {
        tape.scratch.push_str(&part.scratch);
        targets.extend(its.iter().map(|it| Target { node: it.node + base, ..*it }));
        docs.push(Doc {
            version: doc.version,
            map: doc.map.map(|it| it + base),
            install_name: doc.install_name + base,
            list: doc.list + base,
            platform: doc.platform + base,
            targets: doc.targets.start + first..doc.targets.end + first,
        });
    }
    Ok((tape, docs, targets))
}

/// The documents and what they list, read; the lists the output's target selects are
/// gathered into a [`Plan`] before any name is decoded.
struct Stream<'s, 'd> {
    view: View<'s>,
    docs: &'d [Doc],
    targets: &'d [Target],
    /// The first document of each install name.
    by_name: HashMap<&'s str, usize>,
}

/// The lists to decode, in the order their names are added, how many names they give at
/// most and how many bytes those take at most, and the re-exported libraries that are not
/// documents of the file.
#[derive(Default)]
struct Plan<'s> {
    lists: Vec<List>,
    names: usize,
    bytes: usize,
    inputs: Vec<&'s str>,
}

/// A list of names, the scalar nodes `names`, read as `what`.
struct List {
    what: What,
    names: Range<u32>,
}

#[derive(Clone, Copy)]
enum What {
    Symbols { kind: SymbolKind, weak: bool },
    /// Each name under each prefix (`OBJC_CLASS_$_`), its leading `_` dropped first when
    /// `strip` (versions 1 and 2 write Objective-C classes and ivars with one).
    Objc { prefixes: &'static [&'static str], strip: bool },
}

impl<'s> Stream<'s, '_> {
    fn name(&self, doc: &Doc) -> &'s str {
        self.view.str(doc.install_name)
    }

    /// Plans document `at`'s lists, and those of every library it re-exports that is a
    /// document of the file; the others' install names go to `plan.inputs`.
    fn fold(
        &self,
        at: usize,
        arch: Arch,
        platform: u32,
        seen: &mut HashSet<&'s str>,
        plan: &mut Plan<'s>,
    ) -> Result<(), String> {
        let doc = &self.docs[at];
        let chosen = self.choose(doc, arch, platform)?;
        let reexported = self.exports(doc, chosen, platform, plan)?;
        for library in reexported {
            if !seen.insert(library) {
                continue;
            }
            match self.by_name.get(library) {
                Some(&index) => self.fold(index, arch, platform, seen, plan)?,
                None => plan.inputs.push(library),
            }
        }
        Ok(())
    }

    /// The arch this document is read for: `arch` itself when listed on `platform`, else the
    /// ABI-compatible arch listed there that ld64 would take.
    fn choose(&self, doc: &Doc, arch: Arch, platform: u32) -> Result<&'s str, String> {
        self.targets[doc.targets.start as usize..doc.targets.end as usize]
            .iter()
            .filter(|it| it.platform == platform)
            .filter_map(|it| {
                let name = &self.view.str(it.node)[..it.len as usize];
                arch_rank(arch, name).map(|rank| (rank, name))
            })
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, it)| it)
            .ok_or_else(|| {
                // alloc: error path
                format!(
                    "`{}` has no {}-{}; it has {}",
                    self.name(doc),
                    arch_name(arch),
                    platform_name(platform),
                    self.listed(doc)
                )
            })
    }

    /// What the document lists, as written: `targets`, or each of `archs` with the platform.
    fn listed(&self, doc: &Doc) -> String {
        // alloc: error path: what the document lists, for the message
        let mut out = String::new();
        for (index, node) in self.view.scalars(doc.list, "targets").unwrap_or(0..0).enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(self.view.str(node));
            if doc.version != 4 {
                out.push_str(" (");
                out.push_str(self.view.str(doc.platform));
                out.push(')');
            }
        }
        out
    }

    /// Checks and plans the lists of `doc` for `chosen`, and returns the libraries it
    /// re-exports, in order.
    fn exports(
        &self,
        doc: &Doc,
        chosen: &str,
        platform: u32,
        plan: &mut Plan<'s>,
    ) -> Result<Vec<&'s str>, String> {
        let view = self.view;
        // alloc: empty unless the document re-exports; once per document
        let mut libraries = Vec::new();
        if doc.version == 4 {
            for key in ["exports", "reexports"] {
                for section in self.sections(doc, key)? {
                    if self.selects(doc, section, "targets", chosen, platform)? {
                        self.check(doc, key, section, &mut libraries, plan)?;
                    }
                }
            }
            for section in self.sections(doc, "reexported-libraries")? {
                if self.selects(doc, section, "targets", chosen, platform)? {
                    for (key, value) in view.entries(section) {
                        match view.str(key) {
                            "targets" => {}
                            "libraries" => libraries
                                .extend(view.scalars(value, "libraries")?.map(|it| view.str(it))),
                            other => return Err(self.unknown(doc, "reexported-libraries", other)),
                        }
                    }
                }
            }
        } else {
            for section in self.sections(doc, "exports")? {
                if self.selects(doc, section, "archs", chosen, platform)? {
                    self.check(doc, "exports", section, &mut libraries, plan)?;
                }
            }
        }
        Ok(libraries)
    }

    /// Checks one selected list's keys and values and adds its names to `plan`, without
    /// decoding any.
    fn check(
        &self,
        doc: &Doc,
        list: &str,
        section: u32,
        libraries: &mut Vec<&'s str>,
        plan: &mut Plan<'s>,
    ) -> Result<(), String> {
        let view = self.view;
        let v4 = doc.version == 4;
        let strip = doc.version <= 2;
        for (key, value) in view.entries(section) {
            let key = view.str(key);
            let what = match key {
                "targets" if v4 => continue,
                "archs" if !v4 => continue,
                "allowable-clients" | "allowed-clients" => continue,
                "re-exports" if !v4 => {
                    libraries.extend(view.scalars(value, key)?.map(|it| view.str(it)));
                    continue;
                }
                "symbols" => What::Symbols { kind: SymbolKind::Other, weak: false },
                "weak-symbols" if v4 => What::Symbols { kind: SymbolKind::Other, weak: true },
                "weak-def-symbols" if !v4 => What::Symbols { kind: SymbolKind::Other, weak: true },
                "thread-local-symbols" => What::Symbols { kind: SymbolKind::Tls, weak: false },
                "objc-classes" => What::Objc { prefixes: &["OBJC_CLASS_$_", "OBJC_METACLASS_$_"], strip },
                "objc-eh-types" => What::Objc { prefixes: &["OBJC_EHTYPE_$_"], strip: false },
                "objc-ivars" => What::Objc { prefixes: &["OBJC_IVAR_$_"], strip },
                other => return Err(self.unknown(doc, list, other)),
            };
            let names = view.scalars(value, key)?;
            // clone: a `Range<u32>`, two integers
            let text: usize = names.clone().map(|it| view.str(it).len()).sum();
            match what {
                What::Symbols { .. } => {
                    plan.names += names.len();
                    plan.bytes += text + names.len();
                }
                What::Objc { prefixes, .. } => {
                    plan.names += names.len() * prefixes.len();
                    plan.bytes += prefixes.iter().map(|it| it.len() * names.len() + text).sum::<usize>();
                }
            }
            plan.lists.push(List { what, names });
        }
        Ok(())
    }

    /// The mappings of the block sequence under `key`; none when the key is absent.
    fn sections(&self, doc: &Doc, key: &str) -> Result<Children<'s>, String> {
        let view = self.view;
        let Some(value) = doc.map.and_then(|map| view.get(map, key)) else {
            return Ok(Children::empty(view));
        };
        match view.kind(value) {
            Kind::Seq => {
                if view.children(value).any(|it| view.kind(it) != Kind::Map) {
                    // alloc: error path
                    return Err(format!("`{}`: an entry of `{key}` is not a mapping", self.name(doc)));
                }
                Ok(view.children(value))
            }
            Kind::Text | Kind::Scratch if view.str(value).is_empty() => Ok(Children::empty(view)),
            // alloc: error path
            _ => Err(format!("`{}`: `{key}` is not a sequence", self.name(doc))),
        }
    }

    /// Whether a list is one this output reads: its `selector` (`targets` or `archs`) names
    /// `chosen` on `platform` (in v1 to v3 the document's platform, which `choose` checked).
    fn selects(
        &self,
        doc: &Doc,
        section: u32,
        selector: &str,
        chosen: &str,
        platform: u32,
    ) -> Result<bool, String> {
        let view = self.view;
        let value = view
            .get(section, selector)
            // alloc: error path
            .ok_or_else(|| format!("`{}`: a list without `{selector}`", self.name(doc)))?;
        let names = view.scalars(value, selector)?;
        if doc.version == 4 {
            for node in names {
                let (arch, number) = v4_target(view.str(node))?;
                if arch == chosen && number == platform {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            Ok(names.into_iter().any(|node| view.str(node) == chosen))
        }
    }

    fn unknown(&self, doc: &Doc, list: &str, key: &str) -> String {
        // alloc: error path
        format!("`{}`: unknown key `{key}` in `{list}`", self.name(doc))
    }
}

/// The planned lists' names, decoded in order: each one's core name (`_x` is `x`, a name
/// without `_` gets `\x01`, as `super::core_name` makes them) borrowed from the text or built
/// in one reused buffer, hashed once, and appended to the arena only when new. The symbols,
/// the arena and the set are each sized once from the plan, so nothing grows per name.
fn decode(view: View<'_>, plan: &Plan<'_>) -> Result<Exports, String> {
    let mut out = Exports {
        symbols: Vec::with_capacity(plan.names),
        names: Names::with_capacity(plan.bytes),
        seen: Seen::with_capacity(plan.names),
    };
    // alloc: one buffer per file, reused for every name that needs a prefix
    let mut key = String::new();
    for list in &plan.lists {
        // clone: a `Range<u32>`, two integers
        for node in list.names.clone() {
            let name = view.str(node);
            match list.what {
                What::Symbols { kind, weak } => match name.strip_prefix('_') {
                    Some(rest) => out.add(rest, kind, weak)?,
                    None => {
                        key.clear();
                        key.push('\x01');
                        key.push_str(name);
                        out.add(&key, kind, weak)?;
                    }
                },
                What::Objc { prefixes, strip } => {
                    let name = if strip { name.strip_prefix('_').unwrap_or(name) } else { name };
                    for prefix in prefixes {
                        key.clear();
                        key.push_str(prefix);
                        key.push_str(name);
                        out.add(&key, SymbolKind::Other, false)?;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The exports so far, each name once: 16 bytes of [`SharedSymbol`] and the name's bytes in
/// `names` per export, 8 to 16 bytes of `seen` per planned name.
struct Exports {
    symbols: Vec<SharedSymbol>,
    names: Names,
    seen: Seen,
}

impl Exports {
    fn add(&mut self, name: &str, kind: SymbolKind, weak: bool) -> Result<(), String> {
        if self.seen.insert(&self.symbols, &self.names, name) {
            let name = self.names.push(name)?;
            self.symbols.push(SharedSymbol { name, version: None, kind, weak });
        }
        Ok(())
    }
}

/// The names of `symbols`, by open addressing: a slot (4 bytes) is an index into `symbols`
/// or [`EMPTY`], at most half the slots are full, so a lookup hashes once and probes about
/// 1.5 slots, comparing against the name's bytes in the arena in place.
struct Seen {
    slots: Vec<u32>,
    mask: usize,
}

const EMPTY: u32 = u32::MAX;

impl Seen {
    fn with_capacity(names: usize) -> Seen {
        let len = (names.max(8) * 2).next_power_of_two();
        // alloc: one table per file, sized once from the plan's name count
        Seen { slots: vec![EMPTY; len], mask: len - 1 }
    }

    /// Whether `name` is not among `symbols` (named in `names`); when it is not, it is
    /// recorded as the symbol the caller pushes next.
    fn insert(&mut self, symbols: &[SharedSymbol], names: &Names, name: &str) -> bool {
        if symbols.len() * 2 >= self.slots.len() {
            self.grow(symbols, names);
        }
        let mut at = FixedState.hash_one(name) as usize & self.mask;
        loop {
            match self.slots[at] {
                EMPTY => {
                    self.slots[at] = symbols.len() as u32;
                    return true;
                }
                index if names.at(symbols[index as usize].name) == name => return false,
                _ => at = (at + 1) & self.mask,
            }
        }
    }

    fn grow(&mut self, symbols: &[SharedSymbol], names: &Names) {
        let len = self.slots.len() * 2;
        self.slots.clear();
        self.slots.resize(len, EMPTY);
        self.mask = len - 1;
        for (index, symbol) in symbols.iter().enumerate() {
            let mut at = FixedState.hash_one(names.at(symbol.name)) as usize & self.mask;
            while self.slots[at] != EMPTY {
                at = (at + 1) & self.mask;
            }
            self.slots[at] = index as u32;
        }
    }
}

/// A target a document lists: its arch is the first `len` bytes of scalar `node`.
#[derive(Clone, Copy, Debug)]
struct Target {
    node: u32,
    len: u32,
    /// `LC_BUILD_VERSION`'s number; 0 for a platform this reader doesn't know.
    platform: u32,
}

const _: () = assert!(size_of::<Target>() == 12);

/// One document, its version known and its install name read; nodes of the tape.
struct Doc {
    version: u8,
    /// Its mapping; `None` for an empty document.
    map: Option<u32>,
    install_name: u32,
    /// `targets` (v4) or `archs` (v1 to v3), for the error when the output's target is not
    /// among them.
    list: u32,
    /// v1 to v3: `platform`.
    platform: u32,
    /// What it lists, in the stream's targets.
    targets: Range<u32>,
}

impl Doc {
    fn new(view: &View<'_>, raw: &RawDoc<'_>, root: u32, targets: &mut Vec<Target>) -> Result<Doc, String> {
        let first = if raw.lines.is_empty() { 0 } else { raw.number };
        let map = match view.kind(root) {
            Kind::Map => Some(root),
            Kind::Text | Kind::Scratch if view.str(root).is_empty() => None,
            // alloc: error path
            _ => return Err(format!("line {first}: a document must be a mapping")),
        };
        let get = |key: &str| map.and_then(|map| view.get(map, key));
        let version = match raw.tag {
            None => 1,
            Some("!tapi-tbd-v2") => 2,
            Some("!tapi-tbd-v3") => 3,
            Some("!tapi-tbd") => {
                let version = get("tbd-version")
                    // alloc: error path
                    .ok_or_else(|| format!("line {first}: `!tapi-tbd` without `tbd-version`"))?;
                match view.scalar(version, "tbd-version")? {
                    "4" => 4,
                    other => {
                        // alloc: error path
                        return Err(format!(
                            "line {first}: tbd-version {other} is not read: only versions 1 to 4"
                        ));
                    }
                }
            }
            // alloc: error path
            Some(other) => return Err(format!("line {first}: unknown document tag `{other}`")),
        };
        let install_name = get("install-name")
            // alloc: error path
            .ok_or_else(|| format!("line {first}: a document without `install-name`"))?;
        let name = view.scalar(install_name, "install-name")?;

        let start = targets.len() as u32;
        let (list, platform) = if version == 4 {
            // alloc: error path
            let list = get("targets").ok_or_else(|| format!("`{name}` has no `targets`"))?;
            for node in view.scalars(list, "targets")? {
                let (arch, platform) = v4_target(view.str(node))?;
                targets.push(Target { node, len: arch.len() as u32, platform });
            }
            (list, list)
        } else {
            // alloc: error path
            let archs = get("archs").ok_or_else(|| format!("`{name}` has no `archs`"))?;
            // alloc: error path
            let platform = get("platform").ok_or_else(|| format!("`{name}` has no `platform`"))?;
            let platform_text = view.scalar(platform, "platform")?;
            for node in view.scalars(archs, "archs")? {
                let arch = view.str(node);
                for &number in v3_platforms(platform_text, arch) {
                    targets.push(Target { node, len: arch.len() as u32, platform: number });
                }
            }
            (archs, platform)
        };
        Ok(Doc { version, map, install_name, list, platform, targets: start..targets.len() as u32 })
    }
}

fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::Aarch64 => "arm64",
        Arch::X86_64 => "x86_64",
        Arch::Wasm32 => "wasm32",
    }
}

/// How close a `.tbd` arch is to `arch`: 0 for `arch` itself, more for an ABI-compatible
/// one, `None` for one of another CPU.
fn arch_rank(arch: Arch, name: &str) -> Option<u8> {
    match arch {
        Arch::Aarch64 => match name {
            "arm64" => Some(0),
            "arm64e" => Some(1),
            _ if name.starts_with("arm64e.") => Some(2),
            _ => None,
        },
        Arch::X86_64 => match name {
            "x86_64" => Some(0),
            "x86_64h" => Some(1),
            _ => None,
        },
        Arch::Wasm32 => None,
    }
}

const PLATFORMS: [(&str, u32); 14] = [
    ("macos", 1),
    ("ios", 2),
    ("tvos", 3),
    ("watchos", 4),
    ("bridgeos", 5),
    ("maccatalyst", 6),
    ("ios-simulator", 7),
    ("tvos-simulator", 8),
    ("watchos-simulator", 9),
    ("driverkit", 10),
    ("xros", 11),
    ("xros-simulator", 12),
    ("visionos", 11),
    ("visionos-simulator", 12),
];

fn platform_name(number: u32) -> String {
    PLATFORMS
        .iter()
        .find(|it| it.1 == number)
        // alloc: error path: a platform's name, for the message
        .map_or_else(|| format!("platform {number}"), |it| it.0.to_owned())
}

/// A v4 target, `<arch>-<platform>` (`arm64e.x1-maccatalyst`, `x86_64-ios-simulator`): its
/// arch and its platform's number.
fn v4_target(text: &str) -> Result<(&str, u32), String> {
    let (arch, platform) =
        // alloc: error path
        text.split_once('-').ok_or_else(|| format!("target `{text}` is not `<arch>-<platform>`"))?;
    Ok((arch, PLATFORMS.iter().find(|it| it.0 == platform).map_or(0, |it| it.1)))
}

/// The platforms a v1 to v3 document's `platform` stands for with `arch`: those versions
/// name no simulator, which is the platform on an Intel arch.
fn v3_platforms(name: &str, arch: &str) -> &'static [u32] {
    let intel = matches!(arch, "x86_64" | "x86_64h" | "i386");
    match name {
        "macosx" | "macos" => &[1],
        "ios" if intel => &[7],
        "ios" => &[2],
        "tvos" if intel => &[8],
        "tvos" => &[3],
        "watchos" if intel => &[9],
        "watchos" => &[4],
        "bridgeos" => &[5],
        "iosmac" | "maccatalyst" => &[6],
        "zippered" => &[1, 6],
        "ios-simulator" => &[7],
        "tvos-simulator" => &[8],
        "watchos-simulator" => &[9],
        "driverkit" => &[10],
        "xros" | "visionos" => &[11],
        "xros-simulator" | "visionos-simulator" => &[12],
        _ => &[],
    }
}

// Byte search, eight bytes a step.

const ONES: u64 = 0x0101_0101_0101_0101;

#[inline(always)]
fn load(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// The high bit of each byte of `word` equal to `byte`. Exact up to the lowest one set (a
/// borrow may set a bit above a match), which is the only one a search reads.
#[inline(always)]
fn equal(word: u64, byte: u8) -> u64 {
    let x = word ^ ONES.wrapping_mul(u64::from(byte));
    x.wrapping_sub(ONES) & !x & (ONES << 7)
}

/// The first byte at or after `at` in `set`, or `bytes.len()`.
#[inline]
fn find<const N: usize>(bytes: &[u8], mut at: usize, set: [u8; N]) -> usize {
    while at + 8 <= bytes.len() {
        let word = load(bytes, at);
        let hits = set.iter().fold(0, |hits, &byte| hits | equal(word, byte));
        if hits != 0 {
            return at + (hits.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    while at < bytes.len() && !set.contains(&bytes[at]) {
        at += 1;
    }
    at
}

/// Whether a line's scan outside quotes stops at `byte`: at each byte it acts on (`'`, `"`,
/// `#`, brackets and braces, the newline), and at the few that `| 0x20` folds onto them
/// (`\x02`, `\x03`, `\x07`, `*`), which it steps over.
#[inline(always)]
fn candidate(byte: u8) -> bool {
    let folded = byte | 0x20;
    folded & 0xfe == 0x22 || matches!(folded, 0x27 | 0x2a | 0x7b | 0x7d)
}

/// The first [`candidate`] at or after `at`, or `bytes.len()`: five compares a word where
/// eight bytes would take eight.
#[inline]
fn candidate_at(bytes: &[u8], mut at: usize) -> usize {
    while at + 8 <= bytes.len() {
        let folded = load(bytes, at) | ONES * 0x20;
        let hits = equal(folded & ONES * 0xfe, 0x22)
            | equal(folded, 0x27)
            | equal(folded, 0x2a)
            | equal(folded, 0x7b)
            | equal(folded, 0x7d);
        if hits != 0 {
            return at + (hits.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    while at < bytes.len() && !candidate(bytes[at]) {
        at += 1;
    }
    at
}

// The YAML subset.

/// What a tape node is: a scalar in the text or in the scratch buffer, or a collection whose
/// nodes follow it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Scratch,
    Seq,
    Map,
}

/// One node of the tape, 12 bytes. A scalar is `len` bytes at `at` of the text or the
/// scratch buffer. A sequence or mapping has the index after its subtree in `at` and its
/// item or entry count in `len`; its items (a mapping's keys and values in turn) follow it,
/// so a subtree is skipped in one step.
#[derive(Clone, Copy, Debug)]
struct Node {
    kind: Kind,
    at: u32,
    len: u32,
}

const _: () = assert!(size_of::<Node>() == 12);

/// Every document's nodes, and the scalars that are not a span of the text as written
/// (unescaped, or wrapped across lines).
struct Tape {
    nodes: Vec<Node>,
    scratch: String,
}

/// A scalar not yet on the tape.
#[derive(Clone, Copy, Debug)]
struct Scalar {
    kind: Kind,
    at: usize,
    len: usize,
}

impl Tape {
    fn push(&mut self, kind: Kind, at: usize, len: usize) -> u32 {
        let index = self.nodes.len() as u32;
        self.nodes.push(Node { kind, at: at as u32, len: len as u32 });
        index
    }

    fn scalar(&mut self, scalar: Scalar) -> u32 {
        self.push(scalar.kind, scalar.at, scalar.len)
    }

    fn close(&mut self, node: u32, count: usize) {
        let end = self.nodes.len() as u32;
        let node = &mut self.nodes[node as usize];
        node.at = end;
        node.len = count as u32;
    }

    fn view<'s>(&'s self, text: &'s str) -> View<'s> {
        View { text, scratch: &self.scratch, nodes: &self.nodes }
    }
}

/// The tape, read.
#[derive(Clone, Copy)]
struct View<'s> {
    text: &'s str,
    scratch: &'s str,
    nodes: &'s [Node],
}

impl<'s> View<'s> {
    fn kind(&self, node: u32) -> Kind {
        self.nodes[node as usize].kind
    }

    /// A scalar's text; empty for a collection.
    fn str(&self, node: u32) -> &'s str {
        let Node { kind, at, len } = self.nodes[node as usize];
        let range = at as usize..at as usize + len as usize;
        match kind {
            Kind::Text => &self.text[range],
            Kind::Scratch => &self.scratch[range],
            Kind::Seq | Kind::Map => "",
        }
    }

    /// The node after `node`'s subtree.
    fn after(&self, node: u32) -> u32 {
        let it = self.nodes[node as usize];
        match it.kind {
            Kind::Seq | Kind::Map => it.at,
            Kind::Text | Kind::Scratch => node + 1,
        }
    }

    /// The nodes directly under a collection.
    fn children(&self, node: u32) -> Children<'s> {
        Children { view: *self, at: node + 1, end: self.nodes[node as usize].at }
    }

    fn entries(&self, map: u32) -> Entries<'s> {
        Entries(self.children(map))
    }

    fn get(&self, map: u32, key: &str) -> Option<u32> {
        self.entries(map).find(|&(it, _)| self.str(it) == key).map(|(_, value)| value)
    }

    fn scalar(&self, value: u32, key: &str) -> Result<&'s str, String> {
        match self.kind(value) {
            Kind::Text | Kind::Scratch => Ok(self.str(value)),
            // alloc: error path
            Kind::Seq | Kind::Map => Err(format!("`{key}` is not a scalar")),
        }
    }

    /// A sequence of scalars, as the range of their nodes; a lone scalar is a sequence of
    /// one, an empty value of none.
    fn scalars(&self, value: u32, key: &str) -> Result<Range<u32>, String> {
        let node = self.nodes[value as usize];
        match node.kind {
            Kind::Text | Kind::Scratch if node.len == 0 => Ok(value..value),
            Kind::Text | Kind::Scratch => Ok(value..value + 1),
            Kind::Seq => {
                let items = value + 1..node.at;
                let all = self.nodes[items.start as usize..items.end as usize]
                    .iter()
                    .all(|it| matches!(it.kind, Kind::Text | Kind::Scratch));
                // alloc: error path
                if all { Ok(items) } else { Err(format!("`{key}` is not a scalar")) }
            }
            // alloc: error path
            Kind::Map => Err(format!("`{key}` is not a sequence")),
        }
    }
}

struct Children<'s> {
    view: View<'s>,
    at: u32,
    end: u32,
}

impl<'s> Children<'s> {
    fn empty(view: View<'s>) -> Children<'s> {
        Children { view, at: 0, end: 0 }
    }
}

impl Iterator for Children<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        (self.at < self.end).then(|| {
            let node = self.at;
            self.at = self.view.after(node);
            node
        })
    }
}

/// A mapping's keys and values.
struct Entries<'s>(Children<'s>);

impl Iterator for Entries<'_> {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        let key = self.0.next()?;
        Some((key, self.0.next()?))
    }
}

/// A document as the stream has it: its tag (`!tapi-tbd-v3`), its lines' bytes, the
/// number (from 1) of the first of them, and how many there are.
struct RawDoc<'t> {
    tag: Option<&'t str>,
    lines: Range<usize>,
    number: usize,
    count: usize,
}

/// The documents of a stream, split at `---` and `...` lines.
fn documents(text: &str) -> Result<Vec<RawDoc<'_>>, String> {
    let bytes = text.as_bytes();
    // alloc: once per file, one entry per document
    let mut docs = Vec::new();
    let mut open: Option<RawDoc<'_>> = None;
    let mut at = 0;
    let mut number = 0;
    while at < bytes.len() {
        let newline = find(bytes, at, [b'\n']);
        let next = (newline + 1).min(bytes.len());
        number += 1;
        let end = if newline < bytes.len() && newline > at && bytes[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        };
        let line = &text[at..end];
        if line == "---" || line.starts_with("--- ") || line.starts_with("---\t") {
            if let Some(mut doc) = open.take() {
                doc.lines.end = at;
                doc.count = number - doc.number;
                docs.push(doc);
            }
            let tag = strip_comment(&line[3..])?.trim();
            open = Some(RawDoc {
                tag: (!tag.is_empty()).then_some(tag),
                lines: next..next,
                number: number + 1,
                count: 0,
            });
        } else if line.trim_end() == "..." {
            if let Some(mut doc) = open.take() {
                doc.lines.end = at;
                doc.count = number - doc.number;
                docs.push(doc);
            }
        } else if open.is_none() && !line.starts_with('%') && !strip_comment(line)?.trim().is_empty() {
            open = Some(RawDoc { tag: None, lines: at..at, number, count: 0 });
        }
        at = next;
    }
    if let Some(mut doc) = open {
        doc.lines.end = bytes.len();
        doc.count = number + 1 - doc.number;
        docs.push(doc);
    }
    Ok(docs)
}

/// Whether a quote after `before` (the line up to it) starts a quoted scalar: at the start of
/// a token.
fn opens_quote(before: &str) -> bool {
    before.chars().next_back().is_none_or(|it| it.is_whitespace() || "[{,:-".contains(it))
}

/// `text` without a comment: a `#` at the start or after a space, outside quotes. For the
/// stream's `---` and outside lines; a document's lines are cut in [`logical_lines`].
fn strip_comment(text: &str) -> Result<&str, String> {
    let mut quote = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if chars.peek().is_some_and(|it| it.1 == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\'' | '"' if opens_quote(&text[..at]) => quote = Some(c),
                '#' if text[..at].chars().next_back().is_none_or(char::is_whitespace) => {
                    return Ok(&text[..at]);
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        // alloc: error path
        return Err("an unterminated quoted scalar".to_owned());
    }
    Ok(text)
}

/// A line's bytes, trimmed and its comment cut off. A logical line is one or more, read as
/// if joined by a space.
#[derive(Clone, Copy, Debug)]
struct Piece {
    start: u32,
    end: u32,
}

/// A logical line: blank lines and comments dropped, a flow collection that wraps joined into
/// the line it starts on. Its text is `pieces[first..end]`.
#[derive(Clone, Copy, Debug)]
struct Line {
    number: u32,
    indent: u32,
    first: u32,
    end: u32,
}

const _: () = assert!(size_of::<Line>() == 16);

/// The logical lines of `doc`: one scan of each line for its quotes, its comment and how many
/// flow collections it opens, a word at a time outside quotes.
fn logical_lines(
    text: &str,
    doc: &RawDoc<'_>,
    lines: &mut Vec<Line>,
    pieces: &mut Vec<Piece>,
) -> Result<(), String> {
    lines.clear();
    pieces.clear();
    let bytes = &text.as_bytes()[..doc.lines.end];
    let mut at = doc.lines.start;
    let mut number = doc.number;
    let mut depth = 0i64;
    while at < bytes.len() {
        let start = at;
        let mut quote = 0u8;
        let mut delta = 0i64;
        let mut cut = None;
        let mut next = at;
        let newline = loop {
            let hit = match quote {
                0 => candidate_at(bytes, next),
                b'\'' => find(bytes, next, [b'\'', b'\n']),
                _ => find(bytes, next, [b'"', b'\\', b'\n']),
            };
            let Some(&byte) = bytes.get(hit) else { break hit };
            if byte == b'\n' {
                break hit;
            }
            next = hit + 1;
            match quote {
                0 => match byte {
                    b'\'' | b'"' if opens_quote(&text[start..hit]) => quote = byte,
                    b'#' if text[start..hit].chars().next_back().is_none_or(char::is_whitespace) => {
                        cut = Some(hit);
                        break find(bytes, hit, [b'\n']);
                    }
                    b'[' | b'{' => delta += 1,
                    b']' | b'}' => delta -= 1,
                    _ => {}
                },
                b'\'' => {
                    if bytes.get(next) == Some(&b'\'') {
                        next += 1;
                    } else {
                        quote = 0;
                    }
                }
                _ => {
                    if byte == b'"' {
                        quote = 0;
                    } else if bytes.get(next).is_some_and(|it| *it != b'\n') {
                        next += 1;
                    }
                }
            }
        };
        if quote != 0 {
            // alloc: error path
            return Err(format!("line {number}: an unterminated quoted scalar"));
        }
        let end = match cut {
            Some(cut) => cut,
            None if newline < bytes.len() && newline > start && bytes[newline - 1] == b'\r' => newline - 1,
            None => newline,
        };
        let content = &text[start..end];
        let rest = content.trim_start();
        let trimmed = rest.trim_end();
        if !trimmed.is_empty() {
            let from = start + content.len() - rest.len();
            let piece = Piece { start: from as u32, end: (from + trimmed.len()) as u32 };
            if depth > 0 {
                pieces.push(piece);
                lines.last_mut().expect("an open flow collection has a line").end += 1;
            } else {
                let indent = content.len() - content.trim_start_matches(' ').len();
                if content.as_bytes().get(indent) == Some(&b'\t') {
                    // alloc: error path
                    return Err(format!("line {number}: a tab in the indentation"));
                }
                let first = pieces.len() as u32;
                lines.push(Line { number: number as u32, indent: indent as u32, first, end: first + 1 });
                pieces.push(piece);
            }
            depth += delta;
            if depth < 0 {
                // alloc: error path
                return Err(format!("line {number}: an unopened `]` or `}}`"));
            }
        }
        number += 1;
        at = newline + 1;
    }
    if depth > 0 {
        let number = lines.last().map_or(0, |it| it.number);
        // alloc: error path
        return Err(format!("line {number}: an unclosed `[` or `{{`"));
    }
    Ok(())
}

/// Puts document `doc` on the tape and returns its root node.
fn parse(
    text: &str,
    doc: &RawDoc<'_>,
    tape: &mut Tape,
    lines: &mut Vec<Line>,
    pieces: &mut Vec<Piece>,
) -> Result<u32, String> {
    logical_lines(text, doc, lines, pieces)?;
    let root = tape.nodes.len() as u32;
    if lines.is_empty() {
        tape.push(Kind::Text, 0, 0);
        return Ok(root);
    }
    let mut parser = Parser { text, lines, pieces, tape, at: 0, depth: 0 };
    parser.block()?;
    if let Some(line) = parser.lines.get(parser.at) {
        // alloc: error path
        return Err(format!("line {}: unexpected indentation", line.number));
    }
    Ok(root)
}

/// How deep collections nest before the reader stops: a bound on its recursion.
const DEPTH: u32 = 256;

/// The block structure of a document's logical lines.
struct Parser<'a> {
    text: &'a str,
    lines: &'a mut [Line],
    pieces: &'a mut [Piece],
    tape: &'a mut Tape,
    at: usize,
    depth: u32,
}

/// A place in a logical line: an offset of the text in `pieces[piece]`, at most its end.
#[derive(Clone, Copy, Debug)]
struct Pos {
    piece: usize,
    at: usize,
}

/// A block mapping line's key, and where its value starts.
struct Split {
    key: Scalar,
    value: Pos,
}

impl Parser<'_> {
    fn head(&self, line: Line) -> &str {
        let piece = self.pieces[line.first as usize];
        &self.text[piece.start as usize..piece.end as usize]
    }

    /// Whether `line` is a block sequence's item. A line that wraps opens a flow collection
    /// in its first piece, so the first piece decides.
    fn is_item(&self, line: Line) -> bool {
        let head = self.head(line);
        head == "-" || head.starts_with("- ")
    }

    fn flow(&mut self, line: Line, at: Pos) -> Flow<'_> {
        Flow {
            text: self.text,
            pieces: &self.pieces[..line.end as usize],
            piece: at.piece,
            at: at.at,
            number: line.number,
            tape: &mut *self.tape,
            depth: self.depth,
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > DEPTH {
            let number = self.lines[self.at].number;
            // alloc: error path
            return Err(format!("line {number}: collections nested deeper than {DEPTH}"));
        }
        Ok(())
    }

    fn block(&mut self) -> Result<(), String> {
        let line = self.lines[self.at];
        if self.is_item(line) {
            self.sequence(line.indent)
        } else if let Some(split) = self.split_key(line)? {
            self.mapping(line.indent, split)
        } else {
            self.at += 1;
            let first = line.first as usize;
            self.value(line, Pos { piece: first, at: self.pieces[first].start as usize })
        }
    }

    /// The flow value of `line` from `at` to its end.
    fn value(&mut self, line: Line, at: Pos) -> Result<(), String> {
        let mut flow = self.flow(line, at);
        flow.value(false)?;
        flow.skip_spaces();
        if !flow.at_end() {
            // alloc: error path
            return Err(format!("line {}: unexpected `{}`", line.number, flow.rest()));
        }
        Ok(())
    }

    fn sequence(&mut self, indent: u32) -> Result<(), String> {
        self.enter()?;
        let seq = self.tape.push(Kind::Seq, 0, 0);
        let mut count = 0;
        let text = self.text;
        while let Some(&line) = self.lines.get(self.at) {
            if line.indent != indent || !self.is_item(line) {
                break;
            }
            let piece = self.pieces[line.first as usize];
            let rest = text[piece.start as usize + 1..piece.end as usize].trim_start();
            if rest.is_empty() {
                self.at += 1;
                self.nested(indent)?;
            } else {
                let start = piece.end as usize - rest.len();
                self.lines[self.at].indent += (start - piece.start as usize) as u32;
                self.pieces[line.first as usize].start = start as u32;
                self.block()?;
            }
            count += 1;
        }
        self.tape.close(seq, count);
        self.depth -= 1;
        Ok(())
    }

    fn mapping(&mut self, indent: u32, first: Split) -> Result<(), String> {
        self.enter()?;
        let map = self.tape.push(Kind::Map, 0, 0);
        let mut count = 0;
        let mut split = Some(first);
        while let Some(&line) = self.lines.get(self.at) {
            if line.indent != indent || self.is_item(line) {
                break;
            }
            let number = line.number;
            let Split { key, value } = match split.take() {
                Some(split) => split,
                None => self
                    .split_key(line)?
                    // alloc: error path
                    .ok_or_else(|| format!("line {number}: expected `key: value`"))?,
            };
            self.at += 1;
            let key = self.tape.scalar(key);
            let last = line.end as usize - 1;
            if value.piece == last && value.at == self.pieces[last].end as usize {
                match self.lines.get(self.at) {
                    Some(&next) if next.indent == indent && self.is_item(next) => self.sequence(indent)?,
                    _ => self.nested(indent)?,
                }
            } else {
                self.value(line, value)?;
            }
            let view = self.tape.view(self.text);
            let name = view.str(key);
            let mut at = map + 1;
            while at < key {
                if view.str(at) == name {
                    // alloc: error path
                    return Err(format!("line {number}: `{name}` given twice"));
                }
                at = view.after(at + 1);
            }
            count += 1;
        }
        self.tape.close(map, count);
        self.depth -= 1;
        Ok(())
    }

    /// The block indented under a line at `indent`, or an empty value when there is none.
    fn nested(&mut self, indent: u32) -> Result<(), String> {
        match self.lines.get(self.at) {
            Some(next) if next.indent > indent => self.block(),
            _ => {
                self.tape.push(Kind::Text, 0, 0);
                Ok(())
            }
        }
    }

    /// A block mapping's `key: value` (the value maybe empty), or `None` for a line that is not one.
    fn split_key(&mut self, line: Line) -> Result<Option<Split>, String> {
        let first = line.first as usize;
        let start = self.pieces[first].start as usize;
        let text = self.text;
        let bytes = text.as_bytes();
        match bytes[start] {
            b'[' | b'{' => Ok(None),
            b'\'' | b'"' => {
                let mut flow = self.flow(line, Pos { piece: first, at: start });
                let key = flow.quoted()?;
                flow.skip_spaces();
                if flow.peek() != Some(b':') {
                    return Ok(None);
                }
                flow.bump();
                if !flow.at_end() && flow.space_len() == 0 {
                    return Ok(None);
                }
                flow.skip_spaces();
                Ok(Some(Split { key, value: flow.pos() }))
            }
            _ => {
                let last = line.end as usize - 1;
                let mut index = first;
                let mut at = start;
                let colon = loop {
                    let end = self.pieces[index].end as usize;
                    let colon = find(&bytes[..end], at, [b':']);
                    if colon < end {
                        if colon + 1 == end || bytes[colon + 1].is_ascii_whitespace() {
                            break colon;
                        }
                        at = colon + 1;
                    } else if index == last {
                        return Ok(None);
                    } else {
                        index += 1;
                        at = self.pieces[index].start as usize;
                    }
                };
                let key = if index == first {
                    Scalar { kind: Kind::Text, at: start, len: text[start..colon].trim_end().len() }
                } else {
                    let scratch = &mut self.tape.scratch;
                    let base = scratch.len();
                    for piece in first..=index {
                        let Piece { start, end } = self.pieces[piece];
                        if piece > first {
                            scratch.push(' ');
                        }
                        let end = if piece == index { colon } else { end as usize };
                        scratch.push_str(&text[start as usize..end]);
                    }
                    Scalar { kind: Kind::Scratch, at: base, len: scratch[base..].trim_end().len() }
                };
                let mut flow = self.flow(line, Pos { piece: index, at: colon + 1 });
                flow.skip_spaces();
                Ok(Some(Split { key, value: flow.pos() }))
            }
        }
    }
}

/// A flow value read over a logical line's pieces, as if they were joined by one space.
struct Flow<'a> {
    text: &'a str,
    /// The pieces up to the line's last.
    pieces: &'a [Piece],
    piece: usize,
    at: usize,
    number: u32,
    tape: &'a mut Tape,
    depth: u32,
}

impl Flow<'_> {
    fn end(&self) -> usize {
        self.pieces[self.piece].end as usize
    }

    fn last(&self) -> bool {
        self.piece + 1 == self.pieces.len()
    }

    fn pos(&self) -> Pos {
        Pos { piece: self.piece, at: self.at }
    }

    fn at_end(&self) -> bool {
        self.at == self.end() && self.last()
    }

    /// The byte at the cursor: a space between two pieces.
    fn peek(&self) -> Option<u8> {
        if self.at < self.end() {
            Some(self.text.as_bytes()[self.at])
        } else if self.last() {
            None
        } else {
            Some(b' ')
        }
    }

    /// Steps over one byte, or the space between two pieces.
    fn bump(&mut self) {
        if self.at < self.end() {
            self.at += 1;
        } else if !self.last() {
            self.piece += 1;
            self.at = self.pieces[self.piece].start as usize;
        }
    }

    fn peek_char(&self) -> Option<char> {
        if self.at < self.end() {
            self.text[self.at..].chars().next()
        } else if self.last() {
            None
        } else {
            Some(' ')
        }
    }

    fn bump_char(&mut self, c: char) {
        if self.at < self.end() {
            self.at += c.len_utf8();
        } else {
            self.bump();
        }
    }

    /// The length of the whitespace character at the cursor; 0 when there is none.
    fn space_len(&self) -> usize {
        match self.peek() {
            Some(byte) if byte < 0x80 => usize::from(byte == b' ' || (9..=13).contains(&byte)),
            Some(_) => self.peek_char().filter(|it| it.is_whitespace()).map_or(0, char::len_utf8),
            None => 0,
        }
    }

    fn skip_spaces(&mut self) {
        loop {
            let len = self.space_len();
            if len == 0 {
                return;
            }
            if self.at < self.end() {
                self.at += len;
            } else {
                self.bump();
            }
        }
    }

    /// The line from the cursor to its end, for an error.
    fn rest(&self) -> String {
        // alloc: error path: the rest of the line, for the message
        let mut out = self.text[self.at..self.end()].to_owned();
        for piece in &self.pieces[self.piece + 1..] {
            out.push(' ');
            out.push_str(&self.text[piece.start as usize..piece.end as usize]);
        }
        out
    }

    fn error(&self, what: &str) -> String {
        // alloc: error path
        format!("line {}: {what} at `{}`", self.number, self.rest())
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > DEPTH {
            // alloc: error path
            return Err(format!("line {}: collections nested deeper than {DEPTH}", self.number));
        }
        Ok(())
    }

    /// Puts one value on the tape.
    fn value(&mut self, in_flow: bool) -> Result<(), String> {
        self.skip_spaces();
        match self.peek() {
            Some(b'[') => {
                self.bump();
                self.enter()?;
                let seq = self.tape.push(Kind::Seq, 0, 0);
                let mut count = 0;
                loop {
                    self.skip_spaces();
                    if self.peek() == Some(b']') {
                        self.bump();
                        break;
                    }
                    self.value(true)?;
                    count += 1;
                    self.skip_spaces();
                    match self.peek() {
                        Some(b',') => self.bump(),
                        Some(b']') => {
                            self.bump();
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `]`")),
                    }
                }
                self.tape.close(seq, count);
                self.depth -= 1;
            }
            Some(b'{') => {
                self.bump();
                self.enter()?;
                let map = self.tape.push(Kind::Map, 0, 0);
                let mut count = 0;
                loop {
                    self.skip_spaces();
                    if self.peek() == Some(b'}') {
                        self.bump();
                        break;
                    }
                    let key = match self.peek() {
                        Some(b'\'' | b'"') => self.quoted()?,
                        _ => self.plain(true, true),
                    };
                    self.tape.scalar(key);
                    self.skip_spaces();
                    if self.peek() != Some(b':') {
                        return Err(self.error("expected `:`"));
                    }
                    self.bump();
                    self.skip_spaces();
                    match self.peek() {
                        Some(b',' | b'}') => {
                            self.tape.push(Kind::Text, 0, 0);
                        }
                        _ => self.value(true)?,
                    }
                    count += 1;
                    self.skip_spaces();
                    match self.peek() {
                        Some(b',') => self.bump(),
                        Some(b'}') => {
                            self.bump();
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `}`")),
                    }
                }
                self.tape.close(map, count);
                self.depth -= 1;
            }
            Some(b'\'' | b'"') => {
                let scalar = self.quoted()?;
                self.tape.scalar(scalar);
            }
            _ => {
                let scalar = self.plain(in_flow, false);
                self.tape.scalar(scalar);
            }
        }
        Ok(())
    }

    /// A plain scalar, trimmed: to the end of the line, or in a flow collection to the next
    /// `,`, `]` or `}` (and `:` for a key). A span of the text unless it wraps a line.
    fn plain(&mut self, in_flow: bool, key: bool) -> Scalar {
        let text = self.text;
        let bytes = text.as_bytes();
        let (first, start) = (self.piece, self.at);
        loop {
            let end = self.end();
            let stop = match (in_flow, key) {
                (true, true) => find(&bytes[..end], self.at, [b',', b']', b'}', b':']),
                (true, false) => find(&bytes[..end], self.at, [b',', b']', b'}']),
                (false, true) => find(&bytes[..end], self.at, [b':']),
                (false, false) => end,
            };
            self.at = stop;
            if stop < end || self.last() {
                break;
            }
            self.piece += 1;
            self.at = self.pieces[self.piece].start as usize;
        }
        let first_end = self.pieces[first].end as usize;
        if self.piece == first || (self.piece == first + 1 && self.at == self.pieces[self.piece].start as usize) {
            let end = if self.piece == first { self.at } else { first_end };
            let span = &text[start..end];
            let rest = span.trim_start();
            return Scalar { kind: Kind::Text, at: start + span.len() - rest.len(), len: rest.trim_end().len() };
        }
        let scratch = &mut self.tape.scratch;
        let base = scratch.len();
        scratch.push_str(&text[start..first_end]);
        for index in first + 1..=self.piece {
            let piece = self.pieces[index];
            let end = if index == self.piece { self.at } else { piece.end as usize };
            scratch.push(' ');
            scratch.push_str(&text[piece.start as usize..end]);
        }
        let span = &scratch[base..];
        let rest = span.trim_start();
        Scalar { kind: Kind::Scratch, at: base + span.len() - rest.len(), len: rest.trim_end().len() }
    }

    /// A quoted scalar at the cursor: a span of the text unless it has an escape or wraps a
    /// line, else unescaped into the scratch buffer.
    fn quoted(&mut self) -> Result<Scalar, String> {
        let text = self.text;
        let bytes = text.as_bytes();
        let quote = bytes[self.at];
        let single = quote == b'\'';
        let search = |bytes: &[u8], at: usize| {
            if single { find(bytes, at, [b'\'']) } else { find(bytes, at, [b'"', b'\\']) }
        };
        self.at += 1;
        let start = self.at;
        let end = self.end();
        let stop = search(&bytes[..end], start);
        if stop < end && bytes[stop] == quote && (!single || stop + 1 == end || bytes[stop + 1] != b'\'') {
            self.at = stop + 1;
            return Ok(Scalar { kind: Kind::Text, at: start, len: stop - start });
        }
        let base = self.tape.scratch.len();
        loop {
            let end = self.end();
            let stop = search(&bytes[..end], self.at);
            self.tape.scratch.push_str(&text[self.at..stop]);
            self.at = stop;
            if stop == end {
                if self.last() {
                    return Err(self.error("an unterminated quoted scalar"));
                }
                self.tape.scratch.push(' ');
                self.bump();
                continue;
            }
            self.at += 1;
            match bytes[stop] {
                b'\'' if self.peek() == Some(b'\'') => {
                    self.at += 1;
                    self.tape.scratch.push('\'');
                }
                b'\'' | b'"' => {
                    return Ok(Scalar { kind: Kind::Scratch, at: base, len: self.tape.scratch.len() - base });
                }
                _ => {
                    let c = self.escape()?;
                    self.tape.scratch.push(c);
                }
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(c) = self.peek_char() else { return Err(self.error("an unterminated escape")) };
        self.bump_char(c);
        let digits = match c {
            '0' => return Ok('\0'),
            't' => return Ok('\t'),
            'n' => return Ok('\n'),
            'r' => return Ok('\r'),
            '"' | '\\' | '/' | ' ' => return Ok(c),
            'x' => 2,
            'u' => 4,
            'U' => 8,
            _ => return Err(self.error("an unknown escape")),
        };
        let bytes = self.text.as_bytes();
        let mut hex = [0u8; 8];
        let (mut piece, mut at) = (self.piece, self.at);
        for slot in &mut hex[..digits] {
            if at < self.pieces[piece].end as usize {
                *slot = bytes[at];
                at += 1;
            } else if piece + 1 < self.pieces.len() {
                *slot = b' ';
                piece += 1;
                at = self.pieces[piece].start as usize;
            } else {
                return Err(self.error("a short escape"));
            }
        }
        if !self.text.is_char_boundary(at) {
            return Err(self.error("a short escape"));
        }
        let code = std::str::from_utf8(&hex[..digits])
            .ok()
            .and_then(|it| u32::from_str_radix(it, 16).ok())
            .ok_or_else(|| self.error("a bad escape"))?;
        (self.piece, self.at) = (piece, at);
        char::from_u32(code).ok_or_else(|| self.error("an escape of no character"))
    }
}

/// [`read`] against the reference reader (`crate::fuzz_tbd_differential`).
#[cfg(feature = "fuzz")]
pub(crate) fn differential(text: &str, arch: Arch, platform: u32) -> Result<bool, String> {
    let new = read("t.tbd", text, arch, platform);
    // The reference recurses once per level with no bound: past the reader's it is not run.
    // alloc: the fuzz differential only
    let nested = format!("collections nested deeper than {DEPTH}");
    if matches!(&new, Err(what) if what.contains(&nested)) {
        return Ok(false);
    }
    let old = reference::read("t.tbd", text, arch, platform).map(|it| (it.symbols, it.name, it.inputs));
    let new = match new {
        Ok(shared) => {
            if let Err(what) = arena_exact(&shared) {
                // alloc: the fuzz differential's failure message
                return Err(format!(
                    "{arch:?} platform {platform}: the reader's arena is not exact ({what}): {shared:?}\n\
                     the reference: {old:?}"
                ));
            }
            Ok((reference::owned(&shared), shared.name, shared.inputs))
        }
        Err(what) => Err(what),
    };
    if new == old {
        Ok(new.is_ok())
    } else {
        // alloc: the fuzz differential's failure message
        Err(format!("{arch:?} platform {platform}: the reader: {new:?}\nthe reference: {old:?}"))
    }
}

/// Every name of `shared` is one [`Names::push`] of the arena, back to back: the arena holds
/// each kept name's bytes once and nothing else, and no symbol has a version.
#[cfg(feature = "fuzz")]
fn arena_exact(shared: &Shared) -> Result<(), String> {
    let mut bytes = 0;
    for symbol in &shared.symbols {
        // alloc: the fuzz differential's failure message
        let name = shared.names.get(symbol.name).ok_or_else(|| format!("{:?} is not a name of the arena", symbol.name))?;
        if symbol.version.is_some() {
            // alloc: the fuzz differential's failure message
            return Err(format!("`{name}` has a version"));
        }
        bytes += name.len();
    }
    if shared.names.bytes() != bytes {
        // alloc: the fuzz differential's failure message
        return Err(format!("the arena holds {} bytes and its names {bytes}", shared.names.bytes()));
    }
    if !shared.versions.is_empty() {
        // alloc: the fuzz differential's failure message
        return Err(format!("versions {:?}", shared.versions));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reference::{Owned, owned};
    use super::*;

    fn sym(name: &str, kind: SymbolKind, weak: bool) -> Owned {
        Owned { name: name.to_owned(), version: None, kind, weak }
    }

    fn names(shared: &Shared) -> Vec<&str> {
        shared.symbols.iter().map(|it| shared.symbol_name(it)).collect()
    }

    const V4: &str = "\
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64-macos,
                   arm64-ios ]
install-name:    '/usr/lib/libfoo.dylib'
current-version: 1.2
# a comment line
reexported-libraries:
  - targets:         [ x86_64-macos, arm64-macos ]
    libraries:       [ '/usr/lib/system/libbar.dylib',
                       \"/usr/lib/libmissing.dylib\" ]
exports:
  - targets:         [ x86_64-macos, arm64-macos ]   # both
    symbols:         [ _foo, '_quoted$name', 'it''s',
                       _bar ]
    objc-classes:    [ NSFoo ]
    weak-symbols:    [ _weakling ]
    thread-local-symbols: [ _tlv ]
  - targets:         [ arm64-ios ]
    symbols:         [ _ios_only ]
  - targets:         [ x86_64-macos ]
    symbols:         [ _intel_only ]
reexports:
  - targets:         [ arm64-macos ]
    symbols:         [ _reexported_sym ]
...
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/system/libbar.dylib'
parent-umbrella:
  - targets:         [ x86_64-macos, arm64e-macos ]
    umbrella:        foo
exports:
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _from_bar, _foo ]
    objc-eh-types:   [ BarErr ]
    objc-ivars:      [ Bar._x ]
...
";

    #[test]
    fn v4_arm64_macos() {
        let shared = read("libfoo.tbd", V4, Arch::Aarch64, 1).unwrap();
        assert_eq!(shared.name, "/usr/lib/libfoo.dylib");
        assert_eq!(
            owned(&shared),
            vec![
                sym("foo", SymbolKind::Other, false),
                sym("quoted$name", SymbolKind::Other, false),
                sym("\x01it's", SymbolKind::Other, false),
                sym("bar", SymbolKind::Other, false),
                sym("OBJC_CLASS_$_NSFoo", SymbolKind::Other, false),
                sym("OBJC_METACLASS_$_NSFoo", SymbolKind::Other, false),
                sym("weakling", SymbolKind::Other, true),
                sym("tlv", SymbolKind::Tls, false),
                sym("reexported_sym", SymbolKind::Other, false),
                sym("from_bar", SymbolKind::Other, false),
                sym("OBJC_EHTYPE_$_BarErr", SymbolKind::Other, false),
                sym("OBJC_IVAR_$_Bar._x", SymbolKind::Other, false),
            ]
        );
        assert_eq!(shared.inputs, vec!["/usr/lib/libmissing.dylib".to_owned()]);
        assert!(shared.more.is_empty());
    }

    #[test]
    fn v4_x86_64_macos() {
        let shared = read("libfoo.tbd", V4, Arch::X86_64, 1).unwrap();
        let names = names(&shared);
        assert!(names.contains(&"intel_only"));
        assert!(names.contains(&"from_bar"));
        assert!(!names.contains(&"reexported_sym"));
        assert!(!names.contains(&"ios_only"));
    }

    #[test]
    fn v4_filters_platform() {
        let shared = read("libfoo.tbd", V4, Arch::Aarch64, 2).unwrap();
        assert_eq!(names(&shared), vec!["ios_only"]);
        assert!(shared.inputs.is_empty());
    }

    #[test]
    fn v4_missing_target() {
        let error = read("libfoo.tbd", V4, Arch::Aarch64, 3).unwrap_err();
        assert!(error.starts_with("libfoo.tbd: "), "{error}");
        assert!(error.contains("arm64-tvos"), "{error}");
        assert!(error.contains("x86_64-macos, arm64-macos, arm64-ios"), "{error}");
    }

    #[test]
    fn v4_exact_arch_wins() {
        let text = "\
--- !tapi-tbd
tbd-version: 4
targets: [ arm64-macos, arm64e-macos ]
install-name: /usr/lib/libexact.dylib
exports:
  - targets: [ arm64-macos ]
    symbols: [ _plain ]
  - targets: [ arm64e-macos ]
    symbols: [ _authenticated ]
...
";
        let shared = read("exact.tbd", text, Arch::Aarch64, 1).unwrap();
        assert_eq!(names(&shared), vec!["plain"]);
    }

    const V3: &str = "\
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
uuids:           [ 'x86_64: 00000000-0000-0000-0000-000000000000',
                   'arm64: 11111111-1111-1111-1111-111111111111' ]
platform:        macosx
install-name:    /usr/lib/libv3.dylib
current-version: 1
exports:
  - archs:           [ x86_64, arm64 ]
    re-exports:      [ /usr/lib/libv3sub.dylib ]
    symbols:         [ _common, \"_double\\x41\" ]
    objc-classes:    [ V3Class ]
    objc-ivars:      [ V3Class.ivar ]
    weak-def-symbols: [ _weak3 ]
    thread-local-symbols: [ _tls3 ]
  - archs:           [ x86_64 ]
    symbols:         [ _x86_only ]
...
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
platform:        macosx
install-name:    /usr/lib/libv3sub.dylib
exports:
  - archs:           [ arm64 ]
    symbols:         [ _sub_arm ]
...
";

    #[test]
    fn v3_arm64_macos() {
        let shared = read("libv3.tbd", V3, Arch::Aarch64, 1).unwrap();
        assert_eq!(shared.name, "/usr/lib/libv3.dylib");
        assert_eq!(
            owned(&shared),
            vec![
                sym("common", SymbolKind::Other, false),
                sym("doubleA", SymbolKind::Other, false),
                sym("OBJC_CLASS_$_V3Class", SymbolKind::Other, false),
                sym("OBJC_METACLASS_$_V3Class", SymbolKind::Other, false),
                sym("OBJC_IVAR_$_V3Class.ivar", SymbolKind::Other, false),
                sym("weak3", SymbolKind::Other, true),
                sym("tls3", SymbolKind::Tls, false),
                sym("sub_arm", SymbolKind::Other, false),
            ]
        );
        assert!(shared.inputs.is_empty());
    }

    #[test]
    fn v3_x86_64_and_missing() {
        let shared = read("libv3.tbd", V3, Arch::X86_64, 1).unwrap();
        let names = names(&shared);
        assert!(names.contains(&"x86_only"));
        assert!(!names.contains(&"sub_arm"));
        let error = read("libv3.tbd", V3, Arch::Aarch64, 2).unwrap_err();
        assert!(error.contains("arm64-ios") && error.contains("arm64 (macosx)"), "{error}");
    }

    #[test]
    fn v2_objc_names_drop_underscore() {
        let text = "\
--- !tapi-tbd-v2
archs: [ x86_64 ]
platform: macosx
install-name: /usr/lib/libv2.dylib
exports:
  - archs: [ x86_64 ]
    objc-classes: [ _Old ]
...
";
        let shared = read("libv2.tbd", text, Arch::X86_64, 1).unwrap();
        assert_eq!(names(&shared), vec!["OBJC_CLASS_$_Old", "OBJC_METACLASS_$_Old"]);
    }

    #[test]
    fn errors() {
        let unknown = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /x\n\
                       exports:\n  - targets: [ arm64-macos ]\n    strange: [ _a ]\n...\n";
        let error = read("u.tbd", unknown, Arch::Aarch64, 1).unwrap_err();
        assert!(error.contains("unknown key `strange`"), "{error}");
        let v5 = "--- !tapi-tbd\ntbd-version: 5\ntargets: [ arm64-macos ]\ninstall-name: /x\n...\n";
        assert!(read("v5.tbd", v5, Arch::Aarch64, 1).unwrap_err().contains("tbd-version 5"));
        assert!(read("j.tbd", "{ \"tapi_tbd_version\": 5 }", Arch::Aarch64, 1).is_err());
        let open = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos\n";
        assert!(read("o.tbd", open, Arch::Aarch64, 1).unwrap_err().contains("unclosed"));
        assert!(read("e.tbd", "", Arch::Aarch64, 1).is_err());
    }

    /// Each document of `text` on the tape, as the reference model's tree; or the first error.
    fn trees(text: &str) -> Result<Vec<(Option<String>, reference::Value)>, String> {
        let mut tape = Tape { nodes: Vec::new(), scratch: String::new() };
        let (mut lines, mut pieces) = (Vec::new(), Vec::new());
        let mut roots = Vec::new();
        for doc in documents(text)? {
            roots.push((doc.tag.map(str::to_owned), parse(text, &doc, &mut tape, &mut lines, &mut pieces)?));
        }
        let view = tape.view(text);
        Ok(roots.into_iter().map(|(tag, root)| (tag, tree(&view, root))).collect())
    }

    fn tree(view: &View<'_>, node: u32) -> reference::Value {
        use reference::Value;
        let it = view.nodes[node as usize];
        match it.kind {
            Kind::Text | Kind::Scratch => Value::Scalar(view.str(node).to_owned()),
            Kind::Seq => {
                let items: Vec<Value> = view.children(node).map(|it| tree(view, it)).collect();
                assert_eq!(items.len(), it.len as usize);
                Value::Seq(items)
            }
            Kind::Map => {
                let entries: Vec<(String, Value)> =
                    view.entries(node).map(|(key, value)| (view.str(key).to_owned(), tree(view, value))).collect();
                assert_eq!(entries.len(), it.len as usize);
                Value::Map(entries)
            }
        }
    }

    /// The reference model's documents, parsed one after another as `trees` does.
    fn reference_trees(text: &str) -> Result<Vec<(Option<String>, reference::Value)>, String> {
        reference::documents(text)?
            .into_iter()
            .map(|(tag, lines)| Ok((tag, reference::parse_block(&lines)?)))
            .collect()
    }

    #[test]
    fn yaml_subset() {
        use reference::Value;
        let text = "\
a: plain value # comment
'b c': [ x, 'y, z', \"q\\\"\" ,
         w ]
d:
- one
- k: v
  l: { m: n, o: [ p ] }
e:
";
        let s = |it: &str| Value::Scalar(it.to_owned());
        let expected = Value::Map(vec![
            ("a".to_owned(), s("plain value")),
            ("b c".to_owned(), Value::Seq(vec![s("x"), s("y, z"), s("q\""), s("w")])),
            (
                "d".to_owned(),
                Value::Seq(vec![
                    s("one"),
                    Value::Map(vec![
                        ("k".to_owned(), s("v")),
                        (
                            "l".to_owned(),
                            Value::Map(vec![
                                ("m".to_owned(), s("n")),
                                ("o".to_owned(), Value::Seq(vec![s("p")])),
                            ]),
                        ),
                    ]),
                ]),
            ),
            ("e".to_owned(), s("")),
        ]);
        assert_eq!(trees(text).unwrap(), vec![(None, expected)]);
        assert_eq!(trees(text), reference_trees(text));
    }

    /// Texts where a scalar wraps a line or the lexer's fast paths end: each read as the
    /// reference model reads it.
    #[test]
    fn edges_match_reference() {
        let texts = [
            "k: [ _a\n     _b, _c ]\n",
            "k: [ _a # c\n     _b ]\n",
            "k: [ _a,\n     ]\n",
            "k: [ 'a''\n     b' ]\n",
            "k: [ a 'b, 'c\n d' ]\n",
            "k: { a: [ b ], 'c': , d }\n",
            "k: \"\\x4\n1\"\n",
            "k: \"\\u00e9\\U0001F600\\0\\t\\/\\ \"\n",
            "k: \"\\q\"\n",
            "k: \"\\xZZ\"\n",
            "k: \"\\uD800\"\n",
            "k: \"\\x4é\"\n",
            "'a\n",
            "k: [ a ] b\n",
            "k: v\n  x: y\n",
            "- a\n-\n  - b\n- c: d\n  e: f\n",
            "a: 1\na: 2\n",
            "a:\n- x\n- y\nb: { }\n",
            "\tk: v\n",
            "k: ]\n",
            "a b: c\nfoo [ x,\n y: ] : z\n",
            "\"k\"   :   v\n'k2':v\n",
            "%YAML 1.2\n--- !tapi-tbd # c\na: b\n...\nloose: line\n---\n...\n",
            "a: b\r\nc: [ d,\r\n  e ]\r\n",
            "a: 'x' # c\nb: \"y\" #c\n",
            "a: x#y\nb: '#'\n",
            "a: [ \"x\\\"\", 'y''' ]\n",
            "a: { k: { j: [ [ ], [ x ] ] } }\n",
            "- - - a\n",
            "a: *b\n'c': \x02\x03\x07\n",
        ];
        for text in texts {
            assert_eq!(trees(text), reference_trees(text), "{text:?}");
            same(text);
        }
    }

    /// Reads `text` with the lexer and with the reference model for a few targets, and
    /// checks they agree: the same symbols, inputs and install name, or the same error.
    fn same(text: &str) {
        for arch in [Arch::Aarch64, Arch::X86_64] {
            for platform in [1, 2, 6] {
                let new = read("t.tbd", text, arch, platform);
                if let Ok(shared) = &new {
                    arena_is_exact(shared);
                }
                let new = new.map(|it| (owned(&it), it.name, it.inputs));
                let old =
                    reference::read("t.tbd", text, arch, platform).map(|it| (it.symbols, it.name, it.inputs));
                assert_eq!(new, old, "{arch:?} platform {platform}:\n{text}");
            }
        }
    }

    struct Rng(blaze_ids::rng::SplitMix64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0.next_u64()
        }

        fn below(&mut self, n: usize) -> usize {
            self.0.index(n)
        }

        fn one_in(&mut self, n: usize) -> bool {
            self.0.one_in(n as u64)
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }

        fn subset<'a>(&mut self, items: &[&'a str]) -> Vec<&'a str> {
            let mut out: Vec<&str> = items.iter().copied().filter(|_| self.one_in(2)).collect();
            if out.is_empty() {
                out.push(self.pick(items));
            }
            out
        }
    }

    const TARGETS: [&str; 7] = [
        "x86_64-macos",
        "arm64-macos",
        "arm64e-macos",
        "arm64-ios",
        "x86_64-maccatalyst",
        "arm64e.x1-macos",
        "x86_64h-macos",
    ];
    const ARCHS: [&str; 4] = ["x86_64", "arm64", "arm64e", "x86_64h"];
    const NAMES: [&str; 20] = [
        "_malloc",
        "_free",
        "_printf",
        "dyld_stub_binder",
        "_OBJC_CLASS_$_NSObject",
        "$ld$hide$os10.4$_foo",
        "_it's",
        "_a b",
        "_a:b",
        "_x#y",
        "_c,d",
        "_[e]",
        "__dup",
        "_é",
        "NSFoo",
        "_Old",
        "Bar._x",
        "_tlv",
        "_q\"uote",
        "_back\\slash",
    ];

    /// `name` as a scalar: plain when it can be, else quoted one way or the other, with
    /// escapes now and then.
    fn render(rng: &mut Rng, name: &str) -> String {
        let plain = !name.is_empty() && !name.contains([',', '[', ']', '{', '}', '#', '\'', '"', ':', ' ', '\\']);
        match rng.below(if plain { 4 } else { 2 }) {
            0 => format!("'{}'", name.replace('\'', "''")),
            1 => {
                let mut out = String::from("\"");
                for c in name.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        'a' if rng.one_in(2) => out.push_str("\\x61"),
                        'é' if rng.one_in(2) => out.push_str("\\u00e9"),
                        c => out.push(c),
                    }
                }
                out.push('"');
                out
            }
            _ => name.to_owned(),
        }
    }

    /// `key: <items>` at `indent`: a flow list on one line or wrapped (with comments now and
    /// then), or a block sequence, nested or not.
    fn list(rng: &mut Rng, out: &mut String, nl: &str, indent: usize, key: &str, items: &[String]) {
        let pad = " ".repeat(indent);
        if !items.is_empty() && rng.one_in(4) {
            let deeper = if rng.one_in(2) { indent + 2 } else { indent };
            out.push_str(&format!("{pad}{key}:{nl}"));
            for item in items {
                out.push_str(&format!("{}- {item}{nl}", " ".repeat(deeper)));
            }
            return;
        }
        out.push_str(&format!("{pad}{key}:{}[ ", " ".repeat(1 + rng.below(6))));
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push(',');
                if rng.one_in(3) {
                    if rng.one_in(3) {
                        out.push_str(" # wrapped");
                    }
                    out.push_str(nl);
                    out.push_str(&" ".repeat(indent + 4));
                } else {
                    out.push(' ');
                }
            }
            out.push_str(item);
        }
        out.push_str(&format!(" ]{nl}"));
    }

    fn library(index: usize) -> String {
        format!("/usr/lib/lib{index}.dylib")
    }

    fn names_for(rng: &mut Rng) -> Vec<String> {
        (0..rng.below(6))
            .map(|_| {
                let name = NAMES[rng.below(NAMES.len())];
                render(rng, name)
            })
            .collect()
    }

    fn libraries_for(rng: &mut Rng, most: usize, count: usize) -> Vec<String> {
        (0..1 + rng.below(most))
            .map(|_| {
                let name = library(rng.below(count + 1));
                render(rng, &name)
            })
            .collect()
    }

    fn doc_v4(rng: &mut Rng, out: &mut String, nl: &str, index: usize, count: usize) {
        let targets = rng.subset(&TARGETS);
        let plain: Vec<String> = targets.iter().map(|it| (*it).to_owned()).collect();
        out.push_str(&format!("--- !tapi-tbd{nl}tbd-version:     4{nl}"));
        list(rng, out, nl, 0, "targets", &plain);
        if rng.one_in(3) {
            out.push_str(&format!("# a comment{nl}{nl}"));
        }
        out.push_str(&format!("install-name:    {}{nl}", render(rng, &library(index))));
        if rng.one_in(2) {
            out.push_str(&format!("current-version: 1.2{nl}"));
        }
        if rng.one_in(3) {
            out.push_str(&format!(
                "parent-umbrella:{nl}  - targets:         [ {} ]{nl}    umbrella:        System{nl}",
                targets.join(", ")
            ));
        }
        if rng.one_in(2) {
            out.push_str(&format!("reexported-libraries:{nl}"));
            out.push_str(&format!("  - targets:         [ {} ]{nl}", rng.subset(&targets).join(", ")));
            let libraries = libraries_for(rng, 3, count);
            list(rng, out, nl, 4, "libraries", &libraries);
        }
        for key in ["exports", "reexports"] {
            if rng.one_in(4) {
                continue;
            }
            out.push_str(&format!("{key}:{nl}"));
            for _ in 0..1 + rng.below(3) {
                out.push_str(&format!("  - targets:         [ {} ]{nl}", rng.subset(&targets).join(", ")));
                for list_key in
                    ["symbols", "weak-symbols", "thread-local-symbols", "objc-classes", "objc-eh-types", "objc-ivars"]
                {
                    if rng.one_in(2) {
                        let items = names_for(rng);
                        list(rng, out, nl, 4, list_key, &items);
                    }
                }
                if rng.one_in(6) {
                    out.push_str(&format!("    allowable-clients: [ foo ]{nl}"));
                }
            }
        }
    }

    fn doc_v3(rng: &mut Rng, out: &mut String, nl: &str, index: usize, count: usize) {
        let tag = rng.pick(&["--- !tapi-tbd-v3", "--- !tapi-tbd-v2", "---"]);
        let archs = rng.subset(&ARCHS);
        out.push_str(&format!("{tag}{nl}archs:           [ {} ]{nl}", archs.join(", ")));
        if rng.one_in(2) {
            let uuids: Vec<String> = archs.iter().map(|it| format!("'{it}: 0000-1111'")).collect();
            list(rng, out, nl, 0, "uuids", &uuids);
        }
        out.push_str(&format!("platform:        {}{nl}", rng.pick(&["macosx", "ios", "iosmac", "zippered"])));
        out.push_str(&format!("install-name:    {}{nl}", render(rng, &library(index))));
        out.push_str(&format!("exports:{nl}"));
        for _ in 0..1 + rng.below(3) {
            out.push_str(&format!("  - archs:           [ {} ]{nl}", rng.subset(&archs).join(", ")));
            if rng.one_in(2) {
                let libraries = libraries_for(rng, 2, count);
                list(rng, out, nl, 4, "re-exports", &libraries);
            }
            for list_key in
                ["symbols", "weak-def-symbols", "thread-local-symbols", "objc-classes", "objc-eh-types", "objc-ivars"]
            {
                if rng.one_in(2) {
                    let items = names_for(rng);
                    list(rng, out, nl, 4, list_key, &items);
                }
            }
        }
    }

    fn generate(rng: &mut Rng) -> String {
        let nl = if rng.one_in(4) { "\r\n" } else { "\n" };
        let mut out = String::new();
        if rng.one_in(5) {
            out.push_str(&format!("%YAML 1.2{nl}"));
        }
        let count = 1 + rng.below(3);
        for index in 0..count {
            if rng.one_in(3) {
                doc_v3(rng, &mut out, nl, index, count);
            } else {
                doc_v4(rng, &mut out, nl, index, count);
            }
            if rng.one_in(4) {
                out.push_str(nl);
            }
            if !rng.one_in(5) {
                out.push_str(&format!("...{nl}"));
            }
        }
        if rng.one_in(2) {
            for _ in 0..1 + rng.below(3) {
                let boundaries: Vec<usize> = (0..=out.len()).filter(|&it| out.is_char_boundary(it)).collect();
                let at = boundaries[rng.below(boundaries.len())];
                if rng.one_in(2) {
                    const BYTES: &[u8] = b"[]{}'\",:#- \n\tx";
                    let byte = BYTES[rng.below(BYTES.len())];
                    out.insert(at, char::from(byte));
                } else if at < out.len() {
                    out.remove(at);
                }
            }
        }
        out
    }

    /// The lexer against the reference model over seeded `.tbd` files: versions 1 to 4,
    /// several documents, re-exports in and out of the file, quoting and escapes, wrapped and
    /// block lists, comments, CRLF, and mutated bytes for the error paths.
    #[test]
    fn differential_seeded() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0x7bd5_eed));
        for _ in 0..600 {
            let text = generate(&mut rng);
            assert_eq!(trees(&text), reference_trees(&text), "{text}");
            same(&text);
        }
        same(V4);
        same(V3);
    }

    type Shape = (Vec<(Kind, u32, u32)>, String, Vec<(u8, Option<u32>, u32, u32, u32, Range<u32>)>, Vec<(u32, u32, u32)>);

    /// What a parse of every document gave, as plain values.
    fn shape(parsed: Result<(Tape, Vec<Doc>, Vec<Target>), String>) -> Result<Shape, String> {
        let (tape, docs, targets) = parsed?;
        Ok((
            tape.nodes.iter().map(|it| (it.kind, it.at, it.len)).collect(),
            tape.scratch,
            docs.iter().map(|it| (it.version, it.map, it.install_name, it.list, it.platform, it.targets.clone())).collect(),
            targets.iter().map(|it| (it.node, it.len, it.platform)).collect(),
        ))
    }

    /// The documents parsed on workers and joined give the tape, docs, targets and first
    /// error of parsing them one after another (A6), over the seeded streams and their
    /// mutations.
    #[test]
    fn documents_parse_alike_on_workers_and_in_turn() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0xa6_d0c5));
        let mut joined = 0;
        for _ in 0..600 {
            let mut text = generate(&mut rng);
            for _ in 0..rng.below(3) {
                text.push_str(&generate(&mut rng));
            }
            let Ok(raw) = documents(&text) else { continue };
            let (serial, parallel) = (shape(parse_all(&text, &raw, false)), shape(parse_all(&text, &raw, true)));
            assert_eq!(serial, parallel, "{text}");
            joined += usize::from(raw.len() > 1 && serial.is_ok());
        }
        assert!(joined > 50, "only {joined} streams of several documents parsed");
        for text in [V4, V3] {
            let raw = documents(text).unwrap();
            assert_eq!(shape(parse_all(text, &raw, false)), shape(parse_all(text, &raw, true)));
        }
    }

    /// A stream past [`PARALLEL`] of many documents re-exporting each other, its documents
    /// parsed on workers, reads as the reference reads it.
    #[test]
    fn a_large_stream_reads_as_the_reference_does() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0x1a6e_d0c5));
        for _ in 0..3 {
            let mut text = String::new();
            let mut index = 0;
            while text.len() < 2 * PARALLEL {
                if rng.one_in(3) {
                    doc_v3(&mut rng, &mut text, "\n", index, 1000);
                } else {
                    doc_v4(&mut rng, &mut text, "\n", index, 1000);
                }
                text.push_str("...\n");
                index += 1;
            }
            assert!(documents(&text).unwrap().len() > 1);
            same(&text);
        }
    }

    #[test]
    fn core_names_match_core_name() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(42));
        for _ in 0..200 {
            let raw = NAMES[rng.below(NAMES.len())];
            let text = format!(
                "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /x\nexports:\n  \
                 - targets: [ arm64-macos ]\n    symbols: [ {} ]\n    objc-classes: [ {} ]\n...\n",
                render(&mut rng, raw),
                render(&mut rng, raw)
            );
            let shared = read("c.tbd", &text, Arch::Aarch64, 1).unwrap();
            let expected = [
                crate::macho::core_name(raw.as_bytes()).unwrap(),
                crate::macho::core_name(format!("_OBJC_CLASS_$_{raw}").as_bytes()).unwrap(),
                crate::macho::core_name(format!("_OBJC_METACLASS_$_{raw}").as_bytes()).unwrap(),
            ];
            let mut unique = Vec::new();
            for name in expected {
                if !unique.contains(&name) {
                    unique.push(name);
                }
            }
            assert_eq!(names(&shared), unique.iter().map(String::as_str).collect::<Vec<_>>(), "{text}");
        }
    }

    #[test]
    fn seen_grows_and_dedups() {
        let mut out = Exports { symbols: Vec::new(), names: Names::default(), seen: Seen::with_capacity(0) };
        for round in 0..2 {
            for index in 0..5000 {
                let name = format!("name{index}");
                let before = out.symbols.len();
                out.add(&name, SymbolKind::Other, false).unwrap();
                assert_eq!(out.symbols.len() > before, round == 0, "{name}");
            }
        }
        assert_eq!(out.symbols.len(), 5000);
        assert!(out.symbols.len() * 2 < out.seen.slots.len());
        for (index, symbol) in out.symbols.iter().enumerate() {
            assert_eq!(out.names.at(symbol.name), format!("name{index}"));
        }
    }

    /// Every name of a read library is one [`Names::push`] of the arena, back to back in
    /// symbol order: the arena holds each kept name's bytes once and nothing else.
    fn arena_is_exact(shared: &Shared) {
        let mut bytes = 0;
        for symbol in &shared.symbols {
            let name = shared.names.get(symbol.name).expect("a name of the arena");
            bytes += name.len();
            assert_eq!(symbol.version, None);
        }
        assert_eq!(shared.names.bytes(), bytes);
        assert!(shared.versions.is_empty());
    }

    /// The plan's bound on the arena's bytes holds, so it is sized once and never grows.
    #[test]
    fn arena_sized_once() {
        for text in [V4, V3] {
            for (arch, platform) in [(Arch::Aarch64, 1), (Arch::X86_64, 1), (Arch::Aarch64, 2)] {
                let Ok(shared) = read("t.tbd", text, arch, platform) else { continue };
                arena_is_exact(&shared);
            }
        }
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0xa7e4a));
        for _ in 0..200 {
            let text = generate(&mut rng);
            let raw = documents(&text);
            let Ok(raw) = raw else { continue };
            let mut tape = Tape { nodes: Vec::new(), scratch: String::new() };
            let (mut lines, mut pieces, mut targets) = (Vec::new(), Vec::new(), Vec::new());
            let mut docs = Vec::new();
            let mut ok = !raw.is_empty();
            for doc in &raw {
                match parse(&text, doc, &mut tape, &mut lines, &mut pieces)
                    .and_then(|root| Doc::new(&tape.view(&text), doc, root, &mut targets))
                {
                    Ok(it) => docs.push(it),
                    Err(_) => ok = false,
                }
            }
            if !ok {
                continue;
            }
            let view = tape.view(&text);
            let mut by_name = HashMap::default();
            for (at, doc) in docs.iter().enumerate() {
                by_name.entry(view.str(doc.install_name)).or_insert(at);
            }
            let stream = Stream { view, docs: &docs, targets: &targets, by_name };
            let mut plan = Plan::default();
            let mut seen = HashSet::default();
            seen.insert(view.str(docs[0].install_name));
            if stream.fold(0, Arch::Aarch64, 1, &mut seen, &mut plan).is_err() {
                continue;
            }
            let out = decode(view, &plan).unwrap();
            assert!(out.symbols.len() <= plan.names);
            assert!(out.names.bytes() <= plan.bytes, "{} > {}: {text}", out.names.bytes(), plan.bytes);
        }
    }

    fn noise(rng: &mut Rng, len: usize) -> Vec<u8> {
        const ALPHABET: &[u8] = b"_abcxyz019 ,:[]{}'\"#\\*\n\r\t\x02\x03\x07\x0a\x80\xff";
        (0..len).map(|_| if rng.one_in(3) { ALPHABET[rng.below(ALPHABET.len())] } else { rng.next() as u8 }).collect()
    }

    /// The word-at-a-time searches against a byte loop, from every offset.
    #[test]
    fn swar_matches_scalar() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(7));
        for _ in 0..300 {
            let len = rng.below(80);
            let bytes = noise(&mut rng, len);
            for at in 0..=bytes.len() {
                let scalar = |set: &[u8]| (at..bytes.len()).find(|&it| set.contains(&bytes[it])).unwrap_or(bytes.len());
                assert_eq!(find(&bytes, at, [b'\n']), scalar(b"\n"));
                assert_eq!(find(&bytes, at, [b'\'', b'\n']), scalar(b"'\n"));
                assert_eq!(find(&bytes, at, [b'"', b'\\', b'\n']), scalar(b"\"\\\n"));
                assert_eq!(find(&bytes, at, [b',', b']', b'}', b':']), scalar(b",]}:"));
                let candidates = (at..bytes.len()).find(|&it| candidate(bytes[it])).unwrap_or(bytes.len());
                assert_eq!(candidate_at(&bytes, at), candidates, "{bytes:?} from {at}");
            }
        }
        let acted_on = b"'\"#[]{}\n";
        for byte in 0..=255u8 {
            if acted_on.contains(&byte) {
                assert!(candidate(byte), "{byte:#x}");
            }
        }
    }

    fn sdk_libsystem() -> Option<(String, String)> {
        let mut roots = Vec::new();
        if let Ok(root) = std::env::var("SDKROOT") {
            roots.push(root);
        }
        roots.push("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".to_owned());
        roots.push(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
                .to_owned(),
        );
        roots.into_iter().find_map(|root| {
            let path = format!("{root}/usr/lib/libSystem.B.tbd");
            std::fs::read_to_string(&path).ok().map(|text| (path, text))
        })
    }

    #[test]
    fn sdk_libsystem_tbd() {
        let Some((path, text)) = sdk_libsystem() else {
            eprintln!("no macOS SDK: skipping");
            return;
        };
        let binder = super::super::core_name(b"dyld_stub_binder").unwrap();
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let shared = read(&path, &text, arch, 1).unwrap();
            assert_eq!(shared.name, "/usr/lib/libSystem.B.dylib");
            let names: HashSet<&str> = names(&shared).into_iter().collect();
            for name in ["malloc", "printf", "pthread_create", "_tlv_bootstrap", binder.as_str()] {
                assert!(names.contains(name), "{arch:?}: no `{name:?}` in {path}");
            }
        }
        same(&text);
    }

    /// A read's allocations don't grow with its names: the symbols, the arena and the set are
    /// sized once from the plan, the lines and pieces once from the longest document, and the
    /// tape from the text. 16 times the symbols may cost a few doublings of the buffers that
    /// still grow (the tape past one node per 16 bytes, the plan's lists), never one per name.
    #[test]
    fn read_allocation_budget() {
        let text = |count: usize| {
            let mut out = String::from(
                "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /usr/lib/libx.dylib\n\
                 exports:\n  - targets: [ arm64-macos ]\n    symbols: [ ",
            );
            for at in 0..count {
                if at > 0 {
                    out.push_str(",\n                 ");
                }
                out.push_str(&format!("_name{at}"));
            }
            out.push_str(" ]\n...\n");
            out
        };
        let read_counting = |count: usize| {
            let text = text(count);
            let (shared, used) = blaze_alloc_count::measure(|| read("x.tbd", &text, Arch::Aarch64, 1));
            assert_eq!(shared.unwrap().symbols.len(), count);
            used.allocations
        };
        let (small, big) = (read_counting(64), read_counting(64 * 16));
        assert!(small <= 32, "reading 64 names: {small} allocations");
        assert!(big <= small + 4, "16 times the names: {big} allocations, against {small}");
    }
}
