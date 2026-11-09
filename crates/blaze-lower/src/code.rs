//! A crate's code identity ([`code_identity`]): what of its inputs its code can observe, which
//! [`Lowered::identity`](super::Lowered::identity) is a superset of. It is the crate's
//! identity with each comment of a workspace file that no macro can see replaced by its shape:
//! an edit of a doc comment's text (one doctest example of a library, the prose around it)
//! leaves the library's code identity, and so every other example's, as it was.
//!
//! What can observe a comment's text, and so keeps it whole:
//! - a macro whose input holds it: a doc comment inside a macro call, a `macro_rules!` body
//!   or any token tree, or on an item (field, variant, statement, module) that an attribute
//!   macro or a derive is given, directly or through one of its ancestors;
//! - `include_str!` and `include_bytes!`, which read a file's bytes: a crate any file of which
//!   calls one with anything but a string literal naming a file that is not Rust (`.rs`) keeps
//!   its whole text;
//! - an attribute on an out-of-line `mod x;` that is not inert, which could be given the file:
//!   the crate keeps its whole text.
//!
//! A comment no macro sees (any `//` comment, which no token stream holds, and a doc comment
//! outside every macro input) can't change what the code does, except through the positions
//! of the code after it (`line!()`, a panic's location, debug info): its shape stays, the count
//! of its lines and, for a block comment, the bytes and characters of its last line, so every
//! token after it keeps its line and column.
//!
//! An attribute is inert, giving its item to no macro, when it is one of the compiler's own
//! that never runs user code on the item (`inline`, `doc`, `cfg`, `allow`, `repr`, a tool's
//! `rustfmt::skip`), or a `cfg_attr` whose attributes are all inert. `derive` of the standard
//! derives, `test` and `bench` are macros of the prelude a crate can shadow (`use x::Debug`, a
//! glob of another crate, `#[macro_use] extern crate`): they are inert only where no file of the
//! crate could shadow them ([`CodeText::shadows`]).
//!
//! Files of a library source root (the standard library, registry crates) keep their whole
//! text: they rarely change, and their outlines would have to be parsed in full.

use std::alloc::Global;

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use syntax::{AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, WalkEvent, ast};

#[cfg(test)]
mod tests;

const CODE_IDENTITY: &str = "blaze v2 crate code identity 1";
const OWN_CODE: &str = "blaze v2 crate own code 1";
const CODE_TEXT: &str = "blaze v2 file code text 1";

/// What of a file its crate's code can observe ([`code_text`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CodeText {
    /// The text with every comment no macro sees as its shape, the standard derives, `test`
    /// and `bench` taken as inert.
    pub inert: ContentId,
    /// As `inert`, those taken as macros that see their item.
    pub strict: ContentId,
    /// Whether the file could shadow a standard derive, `test` or `bench`: it imports one of
    /// their names from outside `std`, `core`, `alloc` and the crate, globs another crate's
    /// names, defines a macro of one of their names, or takes another crate's macros with
    /// `#[macro_use]`.
    pub shadows: bool,
    /// Whether its crate keeps its whole text: it reads a file's bytes with `include_str!` or
    /// `include_bytes!` that may be Rust, or gives an out-of-line module to an attribute.
    pub whole: bool,
}

/// The code identity of `krate`: [`own_code`] of it and of every crate it depends on,
/// dependencies first, with a stack of its own rather than the thread's, as
/// [`Lowered::identity`](super::Lowered::identity) works its identity out.
pub fn code_identity(db: &Db, krate: Crate) -> ContentId {
    // alloc: once per crate keyed, a crate of its closure each
    let mut known: blaze_hash::HashMap<Crate, ContentId, Global> = blaze_hash::HashMap::default();
    let mut stack = vec![(krate, false)]; // alloc: as above
    while let Some((at, deps_known)) = stack.pop() {
        if known.contains_key(&at) {
            continue;
        }
        let data = at.data(db);
        if !deps_known {
            stack.push((at, true));
            stack.extend(
                data.dependencies
                    .iter()
                    .filter(|it| !known.contains_key(&it.crate_id))
                    .map(|it| (it.crate_id, false)),
            );
            continue;
        }
        let mut hasher = Hasher::new(CODE_IDENTITY);
        hasher.id(&own_code(db, at));
        hasher.update(&(data.dependencies.len() as u64).to_le_bytes());
        for dep in &data.dependencies {
            hasher.id(&known[&dep.crate_id]);
        }
        known.insert(at, hasher.finish());
    }
    known[&krate]
}

/// What of `krate` alone its code can observe: its content ([`Crate::content`]: its root's
/// path, edition, cfg, env, dependencies and the rest), then its source root's files as
/// [`code_text`] has them, each by its path, sorted, with the module the compiler appended to
/// it. A library source root, or a crate any file of which keeps its whole text, is its root's
/// content and its source root's content, as its identity has them.
///
/// A file of the source root that has no text (bytes that are not UTF-8: an object file or
/// archive a build script left in `OUT_DIR`, which [`crate::Lowered::ran`] puts in its
/// package's source root) is its bytes' content ([`Db::file_content`], what `include_bytes!`
/// reads), never its text: `file_text` has none for it.
#[blaze_query::tracked(returns(copy))]
fn own_code(db: &Db, krate: Crate) -> ContentId {
    let data = krate.data(db);
    let root_file = data.root_file_id;
    let root = db.source_root(db.file_source_root(root_file).source_root_id(db));
    let mut hasher = Hasher::new(OWN_CODE);
    hasher.id(&krate.content(db));
    let whole = |hasher: &mut Hasher| {
        hasher.update(&[0]).id(&db.file_content(root_file)).id(&root.content(db));
        hasher.finish()
    };
    let set = root.source_root_ref(db);
    if set.is_library {
        return whole(&mut hasher);
    }
    // alloc: once per workspace crate keyed in a revision, a file of its source root each
    let mut files: Vec<(&str, base_db::FileId, Option<CodeText>)> = Vec::new();
    for file in set.iter() {
        let Some(path) = set.path_for_file(&file).and_then(|it| it.as_path()).map(|it| it.as_str())
        else {
            return whole(&mut hasher);
        };
        let code = match path.ends_with(".rs") {
            true => {
                let gates = krate.syntax_gates(db);
                code_text(db, EditionedFileId::new_gated(db, file, data.edition, gates))
            }
            false => None,
        };
        files.push((path, file, code));
    }
    if files.iter().any(|it| it.2.is_some_and(|code| code.whole)) {
        return whole(&mut hasher);
    }
    files.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let strict = files.iter().any(|it| it.2.is_some_and(|code| code.shadows));
    hasher.update(&[1, u8::from(strict)]);
    hasher
        .opt_part(set.path_for_file(&root_file).and_then(|it| it.as_path()).map(|it| it.as_str()));
    hasher.update(&(files.len() as u64).to_le_bytes());
    for (path, file, code) in &files {
        let appended = match db.has_file_text(*file) {
            true => db.file_text(*file).appended_root_module(db),
            false => None,
        };
        hasher.part(path.as_bytes()).opt_part(appended);
        match code {
            Some(code) => hasher.update(&[1]).id(if strict { &code.strict } else { &code.inert }),
            None => hasher.update(&[0]).id(&db.file_content(*file)),
        };
    }
    hasher.finish()
}

/// [`code_text_of`] of `file`'s parse; none for a file with no text (bytes that are not
/// UTF-8), which [`own_code`] takes by its bytes' content and the compiler refuses to parse.
///
/// The text is read before the parse: a file that loses its text then reruns this from its
/// first input, and its parse, which would read the text it no longer has, is not checked.
#[blaze_query::tracked(returns(copy))]
fn code_text(db: &Db, file: EditionedFileId) -> Option<CodeText> {
    let id = file.file_id(db);
    if !db.has_file_text(id) {
        // Its bytes, read so that it getting text again reruns this.
        _ = db.file_content(id);
        return None;
    }
    let text = db.file_text(id);
    let text = text.text(db);
    let parse = file.parse(db);
    Some(code_text_of(&parse.syntax_node(), text))
}

/// What of a file of text `text`, parsed as `root`, its crate's code can observe (the module's
/// docs).
pub fn code_text_of(root: &SyntaxNode, text: &str) -> CodeText {
    let mut walk = Walk {
        text,
        inert: Masked::new(),
        strict: Masked::new(),
        shadows: false,
        whole: false,
        // alloc: once per file keyed, a node of its depth each
        seen: Vec::new(),
    };
    for event in root.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(node)) => walk.enter(&node),
            WalkEvent::Leave(NodeOrToken::Node(_)) => _ = walk.seen.pop(),
            WalkEvent::Enter(NodeOrToken::Token(token)) => walk.token(&token),
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
    CodeText {
        inert: walk.inert.finish(text),
        strict: walk.strict.finish(text),
        shadows: walk.shadows,
        whole: walk.whole,
    }
}

/// A file's text hashed with some comments as their shape: the text since the last one, then
/// each one's shape.
struct Masked {
    hasher: Hasher,
    /// Where the text not hashed yet starts.
    from: usize,
}

impl Masked {
    fn new() -> Masked {
        Masked { hasher: Hasher::new(CODE_TEXT), from: 0 }
    }

    /// `token`, a comment at `range` of `text`, as its shape: a byte no UTF-8 text has, its
    /// kind, the count of its lines' ends and, for a block comment, the bytes and characters
    /// after its last line's end (all of it on one line): what the line and column of every
    /// token after it are made of.
    fn mask(&mut self, text: &str, token: &SyntaxToken) {
        let range = token.text_range();
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        let comment = &text[start..end];
        self.hasher.update(&text.as_bytes()[self.from..start]);
        let kind: u8 = match token.kind() {
            SyntaxKind::OUTER_DOC_COMMENT => 1,
            SyntaxKind::INNER_DOC_COMMENT => 2,
            _ => 3,
        };
        let block = comment.starts_with("/*");
        self.hasher.update(&[0xFF, kind, u8::from(block)]);
        self.hasher.update(&(comment.matches('\n').count() as u64).to_le_bytes());
        if block {
            let last = comment.rsplit('\n').next().unwrap_or(comment);
            self.hasher
                .update(&(last.len() as u64).to_le_bytes())
                .update(&(last.chars().count() as u64).to_le_bytes());
        }
        self.from = end;
    }

    fn finish(mut self, text: &str) -> ContentId {
        self.hasher.update(&text.as_bytes()[self.from..]);
        self.hasher.update(&(text.len() as u64).to_le_bytes());
        self.hasher.finish()
    }
}

/// [`code_text_of`]'s walk: for each node entered, whether a macro sees what is in it, with the
/// prelude's macros taken as inert and not.
struct Walk<'a> {
    text: &'a str,
    inert: Masked,
    strict: Masked,
    shadows: bool,
    whole: bool,
    /// Of each node entered and not left: whether a macro sees it, with the prelude's derives,
    /// `test` and `bench` inert, and not.
    seen: Vec<(bool, bool)>,
}

impl Walk<'_> {
    fn enter(&mut self, node: &SyntaxNode) {
        let (mut inert, mut strict) = self.seen.last().copied().unwrap_or((false, false));
        let kind = node.kind();
        if matches!(
            kind,
            SyntaxKind::TOKEN_TREE
                | SyntaxKind::MACRO_CALL
                | SyntaxKind::MACRO_RULES
                | SyntaxKind::MACRO_DEF
        ) {
            (inert, strict) = (true, true);
        }
        if !inert {
            for attr in attrs(node) {
                match attr_seen(&attr) {
                    Seen::No => {}
                    Seen::Prelude => strict = true,
                    Seen::Yes => (inert, strict) = (true, true),
                }
            }
        }
        match kind {
            SyntaxKind::USE => self.shadows |= use_shadows(node),
            SyntaxKind::EXTERN_CRATE => self.shadows |= macro_use_extern(node),
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => {
                let name = node
                    .children()
                    .find(|it| it.kind() == SyntaxKind::NAME)
                    .map(|it| it.text().to_string()); // alloc: a macro's name, once per macro definition
                self.shadows |= name.is_some_and(|it| SHADOWABLE.contains(&it.as_str()));
            }
            // `mod x;` given to an attribute that is not inert: the attribute may be given
            // the file's items.
            SyntaxKind::MODULE if node.children().all(|it| it.kind() != SyntaxKind::ITEM_LIST) => {
                self.whole |= attrs(node).any(|attr| !matches!(attr_seen(&attr), Seen::No));
            }
            _ => {}
        }
        self.seen.push((inert, strict));
    }

    fn token(&mut self, token: &SyntaxToken) {
        let (inert, strict) = self.seen.last().copied().unwrap_or((false, false));
        match token.kind() {
            // No token stream holds a `//` comment.
            SyntaxKind::COMMENT => {
                self.inert.mask(self.text, token);
                self.strict.mask(self.text, token);
            }
            SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT => {
                if !inert {
                    self.inert.mask(self.text, token);
                }
                if !strict {
                    self.strict.mask(self.text, token);
                }
            }
            SyntaxKind::IDENT if matches!(token.text(), "include_str" | "include_bytes") => {
                self.whole |= !reads_other_than_rust(token);
            }
            _ => {}
        }
    }
}

/// The standard derives, and the prelude's attribute macros, that a crate can shadow.
const SHADOWABLE: &[&str] = &[
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "Eq",
    "PartialEq",
    "Ord",
    "PartialOrd",
    "Hash",
    "derive",
    "test",
    "bench",
];

/// The standard derives.
const DERIVES: &[&str] =
    &["Clone", "Copy", "Debug", "Default", "Eq", "PartialEq", "Ord", "PartialOrd", "Hash"];

/// The compiler's attributes that give their item to no macro, and that no crate can shadow
/// (a macro of one of these names used as an attribute is ambiguous, an error).
const INERT: &[&str] = &[
    "doc",
    "cfg",
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "deprecated",
    "must_use",
    "inline",
    "cold",
    "track_caller",
    "repr",
    "non_exhaustive",
    "path",
    "macro_use",
    "macro_export",
    "no_mangle",
    "export_name",
    "link_section",
    "link",
    "link_name",
    "link_ordinal",
    "used",
    "automatically_derived",
    "target_feature",
    "ignore",
    "should_panic",
    "no_std",
    "no_core",
    "no_implicit_prelude",
    "recursion_limit",
    "type_length_limit",
    "crate_type",
    "crate_name",
    "feature",
    "windows_subsystem",
    "no_main",
    "no_builtins",
    "naked",
    "optimize",
    "coverage",
    "instruction_set",
    "debugger_visualizer",
    "collapse_debuginfo",
    "may_dangle",
    "fundamental",
    "marker",
    "unstable",
    "stable",
    "rustc_const_stable",
    "rustc_const_unstable",
    "const_trait",
    "do_not_recommend",
    "on_unimplemented",
    "panic_handler",
    "lang",
];

/// The tools whose attributes (`rustfmt::skip`) are inert.
const TOOLS: &[&str] = &["rustfmt", "clippy", "diagnostic", "rustdoc", "rust_analyzer", "blaze"];

/// Whether an attribute gives its item to a macro.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Seen {
    No,
    /// Only if a prelude macro (a standard derive, `test`, `bench`) is shadowed.
    Prelude,
    Yes,
}

impl Seen {
    fn max(self, other: Seen) -> Seen {
        match (self, other) {
            (Seen::Yes, _) | (_, Seen::Yes) => Seen::Yes,
            (Seen::Prelude, _) | (_, Seen::Prelude) => Seen::Prelude,
            _ => Seen::No,
        }
    }
}

/// The attributes of `node`, and the inner ones of the list it holds (a module's, a block's,
/// an `impl`'s), which apply to all of it.
fn attrs(node: &SyntaxNode) -> impl Iterator<Item = ast::Attr> {
    let lists = node.children().filter(|it| {
        matches!(
            it.kind(),
            SyntaxKind::ITEM_LIST
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::EXTERN_ITEM_LIST
                | SyntaxKind::STMT_LIST
        )
    });
    node.children().chain(lists.flat_map(|it| it.children())).filter_map(ast::Attr::cast)
}

fn attr_seen(attr: &ast::Attr) -> Seen {
    attr.meta().map_or(Seen::Yes, |it| meta_seen(&it))
}

fn meta_seen(meta: &ast::Meta) -> Seen {
    match meta {
        ast::Meta::CfgMeta(_) => Seen::No,
        ast::Meta::CfgAttrMeta(it) => {
            it.metas().fold(Seen::No, |seen, it| seen.max(meta_seen(&it)))
        }
        ast::Meta::UnsafeMeta(it) => it.meta().map_or(Seen::Yes, |it| meta_seen(&it)),
        ast::Meta::PathMeta(it) => path_seen(it.path().as_ref(), None),
        ast::Meta::KeyValueMeta(it) => path_seen(it.path().as_ref(), None),
        ast::Meta::TokenTreeMeta(it) => path_seen(it.path().as_ref(), it.token_tree().as_ref()),
    }
}

/// Whether an attribute of path `path` (and the token tree `args` after it) gives its item to
/// a macro.
fn path_seen(path: Option<&ast::Path>, args: Option<&ast::TokenTree>) -> Seen {
    let Some(path) = path else { return Seen::Yes };
    let segments = path_text(path.syntax());
    let mut parts = segments.split("::").filter(|it| !it.is_empty());
    let (Some(first), rest) = (parts.next(), parts.next()) else { return Seen::Yes };
    if rest.is_some() {
        return if TOOLS.contains(&first) { Seen::No } else { Seen::Yes };
    }
    match first {
        _ if INERT.contains(&first) => Seen::No,
        _ if first.starts_with("rustc_") => Seen::No,
        "test" | "bench" => Seen::Prelude,
        "derive" => args.map_or(Seen::Yes, derives_seen),
        _ => Seen::Yes,
    }
}

/// Whether `derive(..)` of the token tree `args` derives anything but the standard derives,
/// each named by its name or its path in `core` or `std`.
fn derives_seen(args: &ast::TokenTree) -> Seen {
    // alloc: once per `derive` of a workspace file keyed, its paths' text
    let mut path = String::new();
    let mut seen = Seen::Prelude;
    let mut end = |path: &mut String| {
        let segments: Vec<&str> = path.trim_start_matches("::").split("::").collect(); // alloc: as `path`
        let standard = match segments.as_slice() {
            [name] => DERIVES.contains(name),
            [krate, .., name] => matches!(*krate, "core" | "std") && DERIVES.contains(name),
            [] => path.is_empty(),
        };
        if !standard {
            seen = Seen::Yes;
        }
        path.clear();
    };
    for element in args.syntax().children_with_tokens() {
        match element {
            NodeOrToken::Token(token) => match token.kind() {
                SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::WHITESPACE
                | SyntaxKind::COMMENT => {}
                SyntaxKind::COMMA => end(&mut path),
                _ => path.push_str(token.text()),
            },
            // A nested tree is no path a derive names.
            NodeOrToken::Node(_) => return Seen::Yes,
        }
    }
    end(&mut path);
    seen
}

/// The text of `node`'s tokens but whitespace and comments: a path as `core::fmt::Debug`.
fn path_text(node: &SyntaxNode) -> String {
    // alloc: once per attribute or import of a workspace file keyed
    let mut text = String::new();
    for token in node.descendants_with_tokens().filter_map(NodeOrToken::into_token) {
        if !token.kind().is_trivia() {
            text.push_str(token.text());
        }
    }
    text
}

/// The crates whose names an import may take without shadowing a prelude macro: the
/// standard library's (where they come from) and the crate's own (whose imports of another
/// crate's names are imports too, caught where they are).
fn own_or_std(first: &str) -> bool {
    matches!(first, "std" | "core" | "alloc" | "crate" | "self" | "super")
}

/// Whether the `use` item `node` could shadow a prelude macro: a name of one imported, or a
/// glob, from a path that starts outside the standard library and the crate.
fn use_shadows(node: &SyntaxNode) -> bool {
    node.descendants().filter(|it| it.kind() == SyntaxKind::USE_TREE).any(|tree| {
        let glob = tree.children_with_tokens().any(|it| it.kind() == SyntaxKind::STAR);
        let list = tree.children().any(|it| it.kind() == SyntaxKind::USE_TREE_LIST);
        if list && !glob {
            return false;
        }
        // The path from the outermost tree in: its first segment is where the names come from.
        let mut first = None;
        let mut last = None;
        for at in std::iter::once(tree.clone())
            .chain(tree.ancestors().skip(1))
            .take_while(|it| it.kind() != SyntaxKind::USE)
        {
            if at.kind() != SyntaxKind::USE_TREE {
                continue;
            }
            if let Some(path) = at.children().find(|it| it.kind() == SyntaxKind::PATH) {
                let text = path_text(&path);
                if last.is_none() {
                    last = text.rsplit("::").next().map(str::to_owned); // alloc: as `path_text`
                }
                first = text.trim_start_matches("::").split("::").next().map(str::to_owned); // alloc: as above
            }
        }
        let from_outside = !first.as_deref().is_some_and(own_or_std);
        let renamed =
            tree.children().find(|it| it.kind() == SyntaxKind::RENAME).map(|it| path_text(&it));
        let name = match &renamed {
            Some(rename) => rename.strip_prefix("as").map(str::to_owned),
            None => last,
        };
        from_outside && (glob || name.is_some_and(|it| SHADOWABLE.contains(&it.as_str())))
    })
}

/// Whether the `extern crate` item `node` takes another crate's macros with `#[macro_use]`.
fn macro_use_extern(node: &SyntaxNode) -> bool {
    let name =
        node.children().find(|it| it.kind() == SyntaxKind::NAME_REF).map(|it| path_text(&it));
    let macro_use = attrs(node).any(|attr| {
        attr.meta()
            .and_then(|meta| match meta {
                ast::Meta::PathMeta(it) => it.path(),
                _ => None,
            })
            .is_some_and(|path| path_text(path.syntax()) == "macro_use")
    });
    macro_use && !name.is_some_and(|it| matches!(it.as_str(), "std" | "core" | "alloc"))
}

/// Whether the `include_str` or `include_bytes` at `token` is a call with one string literal
/// naming a file that is not Rust: `include_str!("../README.md")`.
fn reads_other_than_rust(token: &SyntaxToken) -> bool {
    let mut next = std::iter::successors(token.next_token(), SyntaxToken::next_token)
        .filter(|it| !it.kind().is_trivia());
    let shape = [next.next(), next.next(), next.next()];
    let [Some(bang), Some(open), Some(literal)] = shape else { return false };
    if bang.kind() != SyntaxKind::BANG
        || !matches!(open.kind(), SyntaxKind::L_PAREN | SyntaxKind::L_BRACK | SyntaxKind::L_CURLY)
    {
        return false;
    }
    let Some(value) =
        ast::String::cast(literal).and_then(|it| it.value().ok().map(|it| it.into_owned()))
    else {
        return false;
    };
    let close = next.find(|it| it.kind() != SyntaxKind::COMMA);
    let closed = close.is_some_and(|it| {
        matches!(it.kind(), SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_CURLY)
    });
    closed && !value.ends_with(".rs")
}
