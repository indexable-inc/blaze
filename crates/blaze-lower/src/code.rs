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
//! A plain `//` comment ends its line, so it moves nothing: it and the spaces before it are
//! left out whole, and adding or removing one at the end of a line changes no code text.
//!
//! What else moves no token and no position is left out the same way ([`Masked::blank`],
//! [`code_end`]): the spaces and tabs before a line's end, which nothing is after, and
//! everything after the file's last token that is whitespace or a plain `//` comment, which
//! nothing is after at all (a comment line appended to a file, blank lines at its end, its last
//! line's end). A crate's unit is keyed by these texts ([`crate::Lowered::narrowed`]), so such
//! an edit leaves its key, and every key above it, as it was. An edit that moves a line (a
//! comment on a line of its own before code, a blank line between items) changes the text:
//! positions are in what a unit makes (item-hash.md, sections 12 and 15).
//!
//! A comment the compiler reads the text of keeps it ([`mask`](Masked::mask)): one with a
//! bidirectional control or a carriage return, and a block comment that does not end, which is
//! an error, so no text with one is the text of a file that builds.
//!
//! An attribute is inert, giving its item to no macro, when it is one of the compiler's own
//! that never runs user code on the item (`inline`, `doc`, `cfg`, `allow`, `repr`, a tool's
//! `rustfmt::skip`), or a `cfg_attr` whose attributes are all inert. `derive` of the standard
//! derives, `test` and `bench` are macros of the prelude a crate can shadow (`use x::Debug`, a
//! glob of another crate, `#[macro_use] extern crate`): they are inert only where no file of the
//! crate could shadow them ([`CodeText::shadows`]).
//!
//! Files of a library source root are parsed as outlines, and a code text needs each parsed
//! in full. The standard library's keep their whole text: it is the toolchain's. A registry,
//! git or other library crate has its narrowed identity worked out from a full parse of each
//! of its files, made here and dropped ([`library_narrow`]), once per content of the crate:
//! the driver keeps it in the store under [`library_key`] and reads it back, so only a crate
//! whose files changed is parsed, and one whose comments alone changed gives the identity it
//! had, to itself and to everything above it ([`crate::Lowered::library_narrows`]).
//!
//! # The interface text
//!
//! What a crate that only checks against this one can observe of a file
//! ([`CodeText::interface`]): the code text with each function body no other crate's check
//! can read ([`plain_body`]) as its shape. It is the crate interface identity of
//! docs/v2/plan/design/item-hash.md (section 15) as it can be built today: everything such a
//! dependent observes stays as text (signatures, types, visibility, attributes, re-exports,
//! macro definitions, generic, `const`, `#[inline]` and `impl Trait` bodies, constants), and
//! so do positions, through the shapes: a diagnostic that points into this crate, a location
//! in code instantiated from it and its span tables all read lines and columns, which no
//! side table holds apart from the code yet. [`crate::Lowered::narrowed`] says which unit
//! reads which text.

use std::alloc::Global;

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use syntax::{AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, WalkEvent, ast};

#[cfg(test)]
mod tests;

const CODE_IDENTITY: &str = "blaze v2 crate code identity 1";
const OWN_CODE: &str = "blaze v2 crate own code 1";
/// 2: a comment whose text the compiler reads (a bidirectional control, a carriage return)
/// keeps its text. 3: the spaces before a line's end and what is after the last token are left
/// out, and a block comment that does not end keeps its text.
const CODE_TEXT: &str = "blaze v2 file code text 3";
/// 2: as [`CODE_TEXT`] 3.
const INTERFACE_TEXT: &str = "blaze v2 file interface text 2";
const OWN_NARROW: &str = "blaze v2 crate own narrow 1";

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
    /// What a crate that only checks against this file's crate can observe of it (the module's
    /// docs, "The interface text"): as `inert`, with each function body no other crate's check
    /// can read as its shape.
    pub interface: ContentId,
    /// As `interface`, the standard derives, `test` and `bench` taken as macros that see their
    /// item.
    pub interface_strict: ContentId,
    /// Whether no body of its crate may be left out of its interface: a standard macro a body
    /// calls could mean something else in it (a macro of that name defined or imported, another
    /// crate's macros taken with `#[macro_use]`), or it calls, outside every body, a macro
    /// that could define one.
    pub unplain: bool,
    /// Whether it names blaze's extensions (`blaze`, `fn_info`, `type_info`): what is proved
    /// of its code (an effect, a contract, a function's code identity) may read the bodies of
    /// the crates its crate depends on.
    pub extends: bool,
    /// Whether it may read a file by path (`#[path]`, `include!` and its kin): its crate may be
    /// made from another crate's file.
    pub reaches: bool,
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

/// Whether `krate` can read no file of its source root but its root file: a workspace crate
/// whose root file is [`root_alone_of`]. Its identity
/// ([`Lowered::identity`](super::Lowered::identity)) then leaves the other files of its source root out: a build script's (`build.rs`, with
/// every other target's sources and its own run's `OUT_DIR` in its package's source root) is
/// not made from them, so an edit of `src/main.rs` neither compiles the script again nor
/// links it. A library source root's crate is never asked: its files don't change, and its
/// root would be parsed for nothing.
pub(crate) fn root_alone(db: &Db, krate: Crate) -> bool {
    let data = krate.data(db);
    let root = db.source_root(db.file_source_root(data.root_file_id).source_root_id(db));
    !root.source_root_ref(db).is_library
        && reads_root_alone(db, EditionedFileId::new(db, data.root_file_id, data.edition))
}

/// [`root_alone_of`] of `file`'s parse; false for a file with no text. The text is read
/// before the parse, as [`code_text`] reads it.
#[blaze_query::tracked(returns(copy))]
fn reads_root_alone(db: &Db, file: EditionedFileId) -> bool {
    let id = file.file_id(db);
    if !db.has_file_text(id) {
        _ = db.file_content(id);
        return false;
    }
    _ = db.file_text(id).text(db);
    root_alone_of(&file.parse(db).syntax_node())
}

/// The macros of the standard library a [`root_alone_of`] file may call: none reads a file,
/// and none makes an item.
const PLAIN_MACROS: &[&str] = &[
    "println",
    "print",
    "eprintln",
    "eprint",
    "format",
    "format_args",
    "write",
    "writeln",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "unreachable",
    "todo",
    "unimplemented",
    "vec",
    "matches",
    "dbg",
    "concat",
    "stringify",
    "env",
    "option_env",
    "cfg",
    "line",
    "column",
    "file",
    "module_path",
    "compile_error",
];

/// Whether a crate whose root file parses as `root` can be proved, from that file alone, to
/// read no other file of its source root: no `mod x;`, no `include!`, `include_str!` or
/// `include_bytes!`, and no macro that could make either. What could make one is refused
/// whole, never looked into:
///
/// - a module with no body, or the `mod` keyword anywhere a macro is given tokens;
/// - the name `include`, `include_str` or `include_bytes` anywhere, a call or not;
/// - a macro defined here (`macro_rules!`, `macro`), any `extern crate`, and any macro call
///   but one of [`PLAIN_MACROS`] by its name alone or from `std`, `core` or `alloc`;
/// - an attribute that gives its item to a macro ([`Seen::Yes`]);
/// - an import that could give one of those names another meaning: any glob but one of the
///   standard library's, and any import, from anywhere else, of a name in [`PLAIN_MACROS`]
///   or [`SHADOWABLE`];
/// - in the tokens a macro is given, which are not parsed: an attribute (`#`), `use`,
///   `extern`, and a call of any macro but a [`PLAIN_MACROS`] one by its name alone.
///
/// A build script that only prints, reads variables and starts programs passes; anything
/// refused keeps its source root whole in its crate's identity, as before.
pub(crate) fn root_alone_of(root: &SyntaxNode) -> bool {
    // How many token trees deep the walk is: tokens a macro is given, parsed as nothing.
    let mut given = 0usize;
    for event in root.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(node)) => {
                let alone = match node.kind() {
                    SyntaxKind::TOKEN_TREE => {
                        given += 1;
                        true
                    }
                    SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF | SyntaxKind::EXTERN_CRATE => {
                        false
                    }
                    SyntaxKind::MODULE => {
                        node.children().any(|it| it.kind() == SyntaxKind::ITEM_LIST)
                    }
                    SyntaxKind::MACRO_CALL => node
                        .children()
                        .find(|it| it.kind() == SyntaxKind::PATH)
                        .is_some_and(|path| plain_macro(&path_text(&path))),
                    SyntaxKind::USE => !use_could_rename(&node),
                    _ => ast::Attr::cast(node).is_none_or(|attr| attr_seen(&attr) != Seen::Yes),
                };
                if !alone {
                    return false;
                }
            }
            WalkEvent::Leave(NodeOrToken::Node(node)) => {
                if node.kind() == SyntaxKind::TOKEN_TREE {
                    given -= 1;
                }
            }
            WalkEvent::Enter(NodeOrToken::Token(token)) => {
                let alone = match token.kind() {
                    SyntaxKind::IDENT => {
                        !matches!(token.text(), "include" | "include_str" | "include_bytes")
                    }
                    _ if given == 0 => true,
                    SyntaxKind::MOD_KW
                    | SyntaxKind::USE_KW
                    | SyntaxKind::EXTERN_KW
                    | SyntaxKind::POUND => false,
                    SyntaxKind::BANG => !calls_other_macro(&token),
                    _ => true,
                };
                if !alone {
                    return false;
                }
            }
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
    true
}

/// Whether the macro path `path` (its text, `std::println`) names one of [`PLAIN_MACROS`]: by
/// its name alone, or from `std`, `core` or `alloc`.
fn plain_macro(path: &str) -> bool {
    let path = path.trim_start_matches("::");
    match path.split_once("::") {
        None => PLAIN_MACROS.contains(&path),
        Some((first, rest)) => {
            let last = rest.rsplit("::").next().unwrap_or(rest);
            matches!(first, "std" | "core" | "alloc") && PLAIN_MACROS.contains(&last)
        }
    }
}

/// Whether `bang`, a `!` among the tokens a macro is given, calls a macro that is not one of
/// [`PLAIN_MACROS`] by its name alone: the token before it is a name (a `!` after anything
/// else negates), and that name is no plain macro's or is the end of a path.
fn calls_other_macro(bang: &SyntaxToken) -> bool {
    let mut before = std::iter::successors(bang.prev_token(), SyntaxToken::prev_token)
        .filter(|it| !it.kind().is_trivia());
    let Some(name) = before.next().filter(|it| it.kind() == SyntaxKind::IDENT) else {
        return false;
    };
    !PLAIN_MACROS.contains(&name.text())
        || before.next().is_some_and(|it| it.kind() == SyntaxKind::COLON2)
}

/// Whether the `use` item `node` could give a name of [`PLAIN_MACROS`] or [`SHADOWABLE`]
/// another meaning: a glob, or an import of one of those names (as it is brought in, renamed
/// or not), from a path that does not start in the standard library. The crate's own paths
/// count: one may lead to another crate's names through an import of its own.
fn use_could_rename(node: &SyntaxNode) -> bool {
    let std = |first: &str| matches!(first, "std" | "core" | "alloc");
    node.descendants().filter(|it| it.kind() == SyntaxKind::USE_TREE).any(|tree| {
        let glob = tree.children_with_tokens().any(|it| it.kind() == SyntaxKind::STAR);
        let list = tree.children().any(|it| it.kind() == SyntaxKind::USE_TREE_LIST);
        if list && !glob {
            return false;
        }
        // The path from this tree out: the outermost tree's first segment is where the names
        // come from, this tree's last what is brought in.
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
        if first.as_deref().is_some_and(std) {
            return false;
        }
        let renamed =
            tree.children().find(|it| it.kind() == SyntaxKind::RENAME).map(|it| path_text(&it));
        let name = match &renamed {
            Some(rename) => rename.strip_prefix("as").map(str::to_owned), // alloc: as `path_text`
            None => last,
        };
        glob || name.is_none_or(|it| {
            PLAIN_MACROS.contains(&it.as_str()) || SHADOWABLE.contains(&it.as_str())
        })
    })
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
            true => code_text(db, EditionedFileId::new(db, file, data.edition)),
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

/// What of `krate` alone can be observed, narrower than its bytes, for the keys that leave
/// trivia out ([`crate::Lowered::narrowed`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OwnNarrow {
    /// What its own code can observe of it ([`CodeText::inert`] of each of its files, its
    /// settings, the module appended to its root): none for a crate that keeps its whole text
    /// (a library source root's, one a file of which reads Rust bytes, [`CodeText::whole`]),
    /// which is then known by its whole identity alone.
    pub(crate) code: Option<ContentId>,
    /// What a crate that only checks against it can observe of it ([`CodeText::interface`] of
    /// each file): none where no body may be left out ([`CodeText::unplain`]), and `code`
    /// stands in.
    pub(crate) interface: Option<ContentId>,
    /// [`CodeText::extends`] of any of its files.
    pub(crate) extends: bool,
    /// [`CodeText::reaches`] of any of its files.
    pub(crate) reaches: bool,
}

/// [`OwnNarrow`] of `krate`: the files [`crate::Lowered::identity`] holds of it (its root
/// alone for a [`root_alone`] crate, else every file of its source root by path), each Rust
/// file as its code text or its interface text, any other by its bytes.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn own_narrow(db: &Db, krate: Crate) -> OwnNarrow {
    let data = krate.data(db);
    let root_file = data.root_file_id;
    let root = db.source_root(db.file_source_root(root_file).source_root_id(db));
    if root.source_root_ref(db).is_library {
        return OwnNarrow::WHOLE;
    }
    narrow_from(
        db,
        krate,
        |file| code_text(db, EditionedFileId::new(db, file, data.edition)),
        || reads_root_alone(db, EditionedFileId::new(db, root_file, data.edition)),
    )
}

/// Whether `krate` is a library source root's crate that is narrowed through the store
/// ([`library_narrow`], [`library_key`]): any but the standard library's.
pub(crate) fn library_narrowed(db: &Db, krate: Crate) -> bool {
    let data = krate.data(db);
    let root = db.source_root(db.file_source_root(data.root_file_id).source_root_id(db));
    root.source_root_ref(db).is_library && !data.origin.is_lang()
}

/// [`OwnNarrow`] of `krate`, a library source root's crate ([`library_narrowed`]): as
/// [`own_narrow`], of every file of its source root (its identity holds them all), each Rust
/// file parsed in full for this alone ([`whole_code_text`]). What the driver keeps under
/// [`library_key`]: asked only where the store has none.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn library_narrow(db: &Db, krate: Crate) -> OwnNarrow {
    let edition = krate.data(db).edition;
    narrow_from(
        db,
        krate,
        |file| whole_code_text(db, EditionedFileId::new(db, file, edition)),
        || false,
    )
}

const LIBRARY_KEY: &str = "blaze v2 library crate narrow 1";

/// What [`library_narrow`] of `krate` is kept under: everything it is a function of. The code
/// that makes it (`front_end`, the front end's code identity, which this crate and the
/// parser are of) and the domains of the texts it holds; the crate's settings (its edition
/// among them), its root's text and its source root's content (every file's path and bytes),
/// as its whole identity has them ([`crate::Lowered::identity`]). Not the crates it depends
/// on: its own narrowed identity reads none of them.
pub(crate) fn library_key(db: &Db, krate: Crate, front_end: &ContentId) -> ContentId {
    let data = krate.data(db);
    let root = db.file_source_root(data.root_file_id).source_root_id(db);
    let mut hasher = Hasher::new(LIBRARY_KEY);
    hasher.id(front_end);
    for domain in [CODE_TEXT, INTERFACE_TEXT, OWN_NARROW] {
        hasher.part(domain.as_bytes());
    }
    hasher
        .id(&krate.content(db))
        .id(&db.file_text(data.root_file_id).content(db))
        .id(&db.source_root(root).content(db));
    hasher.finish()
}

impl OwnNarrow {
    /// A crate that keeps its whole text.
    const WHOLE: OwnNarrow =
        OwnNarrow { code: None, interface: None, extends: false, reaches: false };

    /// How many bytes [`OwnNarrow::kept`] is.
    pub(crate) const KEPT: usize = 4 + 1 + 32 + 32;
    const MAGIC: &[u8; 4] = b"BZN1";

    /// It as the store keeps it: [`Self::MAGIC`], a byte of flags (1: it has a code text, 2:
    /// an interface, 4: `extends`, 8: `reaches`), then the code text's hash and the
    /// interface's, zeros for one it lacks.
    pub(crate) fn kept(&self) -> [u8; Self::KEPT] {
        let mut out = [0; Self::KEPT];
        out[..4].copy_from_slice(Self::MAGIC);
        out[4] = u8::from(self.code.is_some())
            | u8::from(self.interface.is_some()) << 1
            | u8::from(self.extends) << 2
            | u8::from(self.reaches) << 3;
        if let Some(code) = &self.code {
            out[5..37].copy_from_slice(&code.0);
        }
        if let Some(interface) = &self.interface {
            out[37..69].copy_from_slice(&interface.0);
        }
        out
    }

    /// What [`OwnNarrow::kept`] wrote as `bytes`: an error saying what is wrong with bytes it
    /// did not write, each of which is one value's alone (a flag unknown, a hash where the
    /// flags say none).
    pub(crate) fn restored(bytes: &[u8]) -> Result<OwnNarrow, String> {
        let Ok(bytes) = <&[u8; Self::KEPT]>::try_from(bytes) else {
            // alloc: error path
            return Err(format!("{} bytes, where one is {}", bytes.len(), Self::KEPT));
        };
        if &bytes[..4] != Self::MAGIC {
            return Err("it does not start as one does".to_owned()); // alloc: error path
        }
        let flags = bytes[4];
        if flags >> 4 != 0 {
            return Err(format!("flags {flags:#x}, of which it knows the low four")); // alloc: error path
        }
        let hash = |at: usize, has: bool| -> Result<Option<ContentId>, String> {
            let mut id = [0; 32];
            id.copy_from_slice(&bytes[at..at + 32]);
            match (has, id == [0; 32]) {
                (true, _) => Ok(Some(ContentId(id))),
                (false, true) => Ok(None),
                // alloc: error path
                (false, false) => Err("a hash where its flags say it has none".to_owned()),
            }
        };
        Ok(OwnNarrow {
            code: hash(5, flags & 1 != 0)?,
            interface: hash(37, flags & 2 != 0)?,
            extends: flags & 4 != 0,
            reaches: flags & 8 != 0,
        })
    }
}

/// [`OwnNarrow`] of `krate` from `text_of`, each of its files' code text: the files
/// [`crate::Lowered::identity`] holds of it (its root alone when `alone` says it reads no
/// other, else every file of its source root by path), each Rust file as its code text or its
/// interface text, any other by its bytes.
fn narrow_from(
    db: &Db,
    krate: Crate,
    text_of: impl Fn(base_db::FileId) -> Option<CodeText>,
    alone: impl FnOnce() -> bool,
) -> OwnNarrow {
    let data = krate.data(db);
    let root_file = data.root_file_id;
    let root = db.source_root(db.file_source_root(root_file).source_root_id(db));
    let set = root.source_root_ref(db);
    let whole = OwnNarrow::WHOLE;
    // Its root by itself: an alias's (a test's, with its harness appended) is not among its
    // source root's files.
    let Some(root_text) = text_of(root_file) else { return whole };
    let alone = alone();
    // alloc: once per workspace crate keyed in a revision, a file of its source root each
    let mut files: Vec<(&str, base_db::FileId, Option<CodeText>)> = Vec::new();
    if !alone {
        for file in set.iter() {
            let Some(path) =
                set.path_for_file(&file).and_then(|it| it.as_path()).map(|it| it.as_str())
            else {
                return whole;
            };
            let code = match path.ends_with(".rs") {
                true => text_of(file),
                false => None,
            };
            files.push((path, file, code));
        }
        files.sort_unstable_by(|a, b| a.0.cmp(b.0));
    }
    let texts = || std::iter::once(root_text).chain(files.iter().filter_map(|it| it.2));
    let extends = texts().any(|it| it.extends);
    let reaches = texts().any(|it| it.reaches);
    if texts().any(|it| it.whole) {
        return OwnNarrow { extends, reaches, ..whole };
    }
    let strict = texts().any(|it| it.shadows);
    let unplain = texts().any(|it| it.unplain);
    let hashed = |interface: bool| {
        let pick = |code: &CodeText| match (interface, strict) {
            (false, false) => code.inert,
            (false, true) => code.strict,
            (true, false) => code.interface,
            (true, true) => code.interface_strict,
        };
        let mut hasher = Hasher::new(OWN_NARROW);
        hasher
            .id(&krate.content(db))
            .update(&[u8::from(interface), u8::from(strict), u8::from(alone)])
            .id(&pick(&root_text))
            .opt_part(db.file_text(root_file).appended_root_module(db));
        hasher.update(&(files.len() as u64).to_le_bytes());
        for (path, file, code) in &files {
            let appended = match db.has_file_text(*file) {
                true => db.file_text(*file).appended_root_module(db),
                false => None,
            };
            hasher.part(path.as_bytes()).opt_part(appended);
            match code {
                Some(code) => hasher.update(&[1]).id(&pick(code)),
                None => hasher.update(&[0]).id(&db.file_content(*file)),
            };
        }
        hasher.finish()
    };
    OwnNarrow {
        code: Some(hashed(false)),
        interface: (!unplain).then(|| hashed(true)),
        extends,
        reaches,
    }
}

/// What `krate`'s code can observe of its workspace file `file` ([`CodeText::strict`], so a
/// crate that shadows a standard derive is covered too), for `blaze why` to tell an edit of
/// comments from one of code ([`crate::Lowered::identity_parts`]). None for a file with no
/// text, and for one whose crate keeps its whole text ([`CodeText::whole`]).
pub(crate) fn observed(db: &Db, krate: Crate, file: base_db::FileId) -> Option<ContentId> {
    let edition = krate.data(db).edition;
    code_text(db, EditionedFileId::new(db, file, edition))
        .filter(|code| !code.whole)
        .map(|code| code.strict)
}

/// [`code_text`] of a library source root's `file`, whose parse the database keeps as an
/// outline: from a full parse made here and dropped, so the database holds no tree for it.
#[blaze_query::tracked(returns(copy))]
fn whole_code_text(db: &Db, file: EditionedFileId) -> Option<CodeText> {
    let (id, edition) = file.unpack(db);
    if !db.has_file_text(id) {
        // Its bytes, read so that it getting text again reruns this.
        _ = db.file_content(id);
        return None;
    }
    let text = db.file_text(id);
    let text: &str = &**text.text(db);
    let parse = syntax::SourceFile::parse(text, edition);
    Some(code_text_of(parse.tree().syntax(), text))
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
    let end = match planted(Plant::Bytes) {
        true => text.len(),
        false => code_end(root),
    };
    let mut walk = Walk {
        text,
        inert: Masked::of(CODE_TEXT, end),
        strict: Masked::of(CODE_TEXT, end),
        shadows: false,
        whole: false,
        interface: Masked::of(INTERFACE_TEXT, end),
        interface_strict: Masked::of(INTERFACE_TEXT, end),
        skip: (0, 0),
        unplain: false,
        extends: false,
        reaches: false,
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
        interface: walk.interface.finish(text),
        interface_strict: walk.interface_strict.finish(text),
        unplain: walk.unplain,
        extends: walk.extends,
        reaches: walk.reaches,
    }
}

const STILL_TEXT: &str = "blaze v2 file still text 1";

/// The still text of a file of text `text`, parsed as `root`: its bytes but what no code text
/// holds whole, and nothing else. Left out is exactly what [`Masked::mask`] leaves of a plain
/// `//` comment (it and the spaces and tabs before it; never its line's end, never a doc
/// comment, a block comment or a comment the compiler reads, [`READ_IN_COMMENTS`]), what
/// [`Masked::blank`] leaves of whitespace (the spaces and tabs before a line's end) and what is
/// after the file's last token ([`code_end`]).
///
/// The invariant: two texts of one still text have every [`CodeText`] equal, each hash and
/// each flag. Their tokens are the same at the same lines and columns: what is left out is
/// before a line's end or after the last token and inside no token, so every other byte is in
/// both, in order; a doc or block comment is whole in both, so its shape is one; a left-out
/// body's shape counts its lines' ends and what is after the last of them, which holds no
/// `//` comment (one would end the line) and no space before a line's end. So a unit keyed by
/// code texts ([`crate::Lowered::narrowed`]) has one key for both. It is the narrowest such
/// text: an edit of a doc comment's words, which a code text may hold as a shape, changes it.
///
/// What the no-op lane compares of a changed file (blaze-cargo's `serve::job::noop`): one
/// parse of it, no database.
pub fn still_text_of(root: &SyntaxNode, text: &str) -> ContentId {
    let mut still = Masked::of(STILL_TEXT, code_end(root));
    let mut next = root.first_token();
    while let Some(token) = next {
        match token.kind() {
            SyntaxKind::WHITESPACE => still.blank(text, &token),
            // Doc comments are of other kinds; a `/* */` one keeps its text here.
            SyntaxKind::COMMENT if token.text().starts_with("//") => still.mask(text, &token),
            // The planted bug: a number's digits are in no still text.
            SyntaxKind::INT_NUMBER if planted(Plant::Stale) => {
                let range = token.text_range();
                still.skip(text, usize::from(range.start()), usize::from(range.end()));
            }
            _ => {}
        }
        next = token.next_token();
    }
    still.finish(text)
}

/// [`still_text_of`] of `text` parsed as a file of `edition`: one parse, made here and dropped.
pub fn still_text(text: &str, edition: span::Edition) -> ContentId {
    let parse = syntax::SourceFile::parse(text, edition);
    still_text_of(parse.tree().syntax(), text)
}

/// Where the code of a file parsed as `root` ends: the end of its last token that is not
/// whitespace or a plain `//` comment, 0 for a file with none. Nothing is after what follows it,
/// so no token and no position moves with it, and no text holds it ([`Masked::end`]): a
/// comment line appended to a file, blank lines at its end and its last line's end are in no
/// key. A doc comment there stays (it is an attribute with no item, an error), and so does a
/// block comment (one that does not end is an error) and a comment the compiler reads
/// ([`READ_IN_COMMENTS`]).
fn code_end(root: &SyntaxNode) -> usize {
    let mut last = root.last_token();
    while let Some(token) = last {
        let left_out = match token.kind() {
            SyntaxKind::WHITESPACE => true,
            SyntaxKind::COMMENT => {
                token.text().starts_with("//") && !token.text().contains(READ_IN_COMMENTS)
            }
            _ => false,
        };
        if !left_out {
            return usize::from(token.text_range().end());
        }
        last = token.prev_token();
    }
    0
}

/// Whether the block comment `comment` (`/* .. */`, nested ones inside it) ends: each `/*`
/// of it is closed, the last by its last two bytes. One that does not is an error
/// (`unterminated block comment`), which a shape would hide.
fn block_ends(comment: &str) -> bool {
    let bytes = comment.as_bytes();
    let (mut depth, mut at) = (0usize, 0usize);
    while at + 1 < bytes.len() {
        match (bytes[at], bytes[at + 1]) {
            (b'/', b'*') => {
                depth += 1;
                at += 2;
            }
            (b'*', b'/') => {
                let Some(less) = depth.checked_sub(1) else { return false };
                depth = less;
                at += 2;
                if depth == 0 {
                    return at == bytes.len();
                }
            }
            _ => at += 1,
        }
    }
    false
}

/// A file's text hashed with some comments as their shape: the text since the last one, then
/// each one's shape.
struct Masked {
    hasher: Hasher,
    /// Where the text not hashed yet starts.
    from: usize,
    /// Where the file's code ends ([`code_end`]): nothing from here on is hashed.
    end: usize,
}

impl Masked {
    fn of(domain: &'static str, end: usize) -> Masked {
        Masked { hasher: Hasher::new(domain), from: 0, end }
    }

    /// The whitespace `token` of `text` with the spaces and tabs before each of its lines'
    /// ends left out: nothing is after them on their line, so they move no token, as the
    /// spaces before a `//` comment move none. Its lines' ends stay, each a line of every
    /// token after it, and so does what is after its last line's end, the column of the next
    /// token, which stays to be hashed (a `//` comment after it leaves it out). Whitespace
    /// with no line's end is left as it is: it is before a token, or before a comment or the
    /// file's end, which leave it out.
    fn blank(&mut self, text: &str, token: &SyntaxToken) {
        let range = token.text_range();
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        if start >= self.end {
            return;
        }
        let space = &text.as_bytes()[start..end];
        let Some(last) = space.iter().rposition(|it| *it == b'\n') else { return };
        self.hasher.update(&text.as_bytes()[self.from..start]);
        // Where the bytes of `space` not hashed yet start, and the byte looked at.
        let (mut kept, mut at) = (0, 0);
        while at <= last {
            // A run of spaces and tabs, which ends at the latest at the last line's end.
            let run = at;
            while matches!(space[at], b' ' | b'\t') {
                at += 1;
            }
            let ends_line =
                space[at] == b'\n' || (space[at] == b'\r' && space.get(at + 1) == Some(&b'\n'));
            if at > run && ends_line {
                self.hasher.update(&space[kept..run]);
                kept = at;
            }
            at += 1;
        }
        self.hasher.update(&space[kept..=last]);
        self.from = start + last + 1;
    }

    /// `start..end` of `text` left out, with nothing in its place: what a planted bug takes
    /// out of a text ([`Plant::Stale`]).
    fn skip(&mut self, text: &str, start: usize, end: usize) {
        self.hasher.update(&text.as_bytes()[self.from..start]);
        self.from = end;
    }

    /// `token`, a comment at `range` of `text`, as its shape: a byte no UTF-8 text has, its
    /// kind, the count of its lines' ends and, for a block comment, the bytes and characters
    /// after its last line's end (all of it on one line): what the line and column of every
    /// token after it are made of. A plain `//` comment has no shape: it is left out, with
    /// the spaces before it.
    fn mask(&mut self, text: &str, token: &SyntaxToken) {
        let range = token.text_range();
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        // After the file's last token nothing is hashed ([`code_end`]).
        if start >= self.end {
            return;
        }
        let comment = &text[start..end];
        // A comment the compiler reads the text of stays text: a bidirectional control in it
        // is denied (`text_direction_codepoint_in_comment`), a carriage return in a doc
        // comment is an error, and so is a block comment that does not end.
        if comment.contains(READ_IN_COMMENTS)
            || (comment.starts_with("/*") && !block_ends(comment))
        {
            return;
        }
        let kind: u8 = match token.kind() {
            SyntaxKind::OUTER_DOC_COMMENT => 1,
            SyntaxKind::INNER_DOC_COMMENT => 2,
            _ => 3,
        };
        let block = comment.starts_with("/*");
        if kind == 3 && !block {
            // A plain `//` comment runs to its line's end, so nothing is after it on its line:
            // it, and the spaces and tabs before it, move no token and are no token. Whether
            // it is there at all is nothing code can observe, so it leaves nothing, where a doc
            // comment (an attribute of its item) and a block comment (the columns after it)
            // leave their shape. The line's end after it is text, as every line's.
            let before = &text.as_bytes()[self.from..start];
            let kept = before.iter().rposition(|it| !matches!(it, b' ' | b'\t')).map_or(0, |at| at + 1);
            self.hasher.update(&before[..kept]);
            self.from = end;
            return;
        }
        self.hasher.update(&text.as_bytes()[self.from..start]);
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

    /// The function body at `start..end` of `text` as its shape: a byte no UTF-8 text has
    /// (and no comment's shape starts with), the count of its lines' ends, and the bytes,
    /// characters and UTF-16 units after its last line's end (all of it on one line): what the
    /// line and column of every token after it are made of, however a column is counted.
    fn shape(&mut self, text: &str, start: usize, end: usize) {
        let body = &text[start..end];
        self.hasher.update(&text.as_bytes()[self.from..start]);
        let last = body.rsplit('\n').next().unwrap_or(body);
        let lines = match planted(Plant::Positions) {
            true => 0,
            false => body.matches('\n').count(),
        };
        self.hasher
            .update(&[0xFE])
            .update(&(lines as u64).to_le_bytes())
            .update(&(last.len() as u64).to_le_bytes())
            .update(&(last.chars().count() as u64).to_le_bytes())
            .update(&(last.encode_utf16().count() as u64).to_le_bytes());
        self.from = end;
    }

    /// The hash of the text with what was left out: the rest of the text, and nothing of its
    /// length, which a comment's own length is part of (until CODE_TEXT 2 it was hashed, so a
    /// comment edit that made the file longer changed every code text). Each shape starts
    /// with a byte no text has and has a fixed length, so no two streams read the same.
    ///
    /// The rest ends where the file's code does ([`code_end`]): what is after its last token
    /// is in no text.
    fn finish(mut self, text: &str) -> ContentId {
        self.hasher.update(&text.as_bytes()[self.from..self.end.max(self.from)]);
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
    /// [`CodeText::interface`] and [`CodeText::interface_strict`].
    interface: Masked,
    interface_strict: Masked,
    /// Where the last body each of the two left out ends: a comment before it is in the body,
    /// part of its shape.
    skip: (usize, usize),
    unplain: bool,
    extends: bool,
    reaches: bool,
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
            SyntaxKind::USE => {
                self.shadows |= use_shadows(node);
                self.unplain |= use_takes(node, |it| PLAIN_MACROS.contains(&it));
            }
            SyntaxKind::EXTERN_CRATE => {
                let taken = macro_use_extern(node);
                self.shadows |= taken;
                self.unplain |= taken;
            }
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => {
                let name = node
                    .children()
                    .find(|it| it.kind() == SyntaxKind::NAME)
                    .map(|it| it.text().to_string()); // alloc: a macro's name, once per macro definition
                self.shadows |= name.as_ref().is_some_and(|it| SHADOWABLE.contains(&it.as_str()));
                // A macro of a standard macro's name, or one that could define a macro.
                self.unplain |= name.is_none_or(|it| PLAIN_MACROS.contains(&it.as_str()))
                    || node
                        .children()
                        .filter(|it| it.kind() == SyntaxKind::TOKEN_TREE)
                        .flat_map(|it| it.descendants_with_tokens())
                        .filter_map(NodeOrToken::into_token)
                        .any(|it| it.kind() == SyntaxKind::MACRO_KW || it.text() == "macro_rules");
            }
            // A macro called where items are (never in a body, whose calls [`plain_body`]
            // judges) that is not one of the standard library's plain ones: what it makes is
            // not known, and it could define a macro of a standard macro's name.
            SyntaxKind::MACRO_CALL if at_items(node) => self.unplain |= !plain_call(node),
            // A body no other crate's check can read, left out of the interface.
            SyntaxKind::BLOCK_EXPR if plain_body(node) => {
                let range = node.text_range();
                let (start, end) = (usize::from(range.start()), usize::from(range.end()));
                if !inert {
                    self.interface.shape(self.text, start, end);
                    self.skip.0 = end;
                }
                if !strict {
                    self.interface_strict.shape(self.text, start, end);
                    self.skip.1 = end;
                }
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
        // A token of a body an interface left out is part of that body's shape.
        let at = usize::from(token.text_range().start());
        let (in_interface, in_interface_strict) = (at >= self.skip.0, at >= self.skip.1);
        match token.kind() {
            // With the planted bug every text holds its file's bytes, comments and all.
            SyntaxKind::COMMENT | SyntaxKind::WHITESPACE if planted(Plant::Bytes) => {}
            // No token stream holds a `//` comment.
            SyntaxKind::COMMENT => {
                self.inert.mask(self.text, token);
                self.strict.mask(self.text, token);
                if in_interface {
                    self.interface.mask(self.text, token);
                }
                if in_interface_strict {
                    self.interface_strict.mask(self.text, token);
                }
            }
            // Nor the spaces before a line's end: a macro is given tokens, and whether two of
            // them have whitespace between them, which a line's end between them says either
            // way.
            SyntaxKind::WHITESPACE => {
                self.inert.blank(self.text, token);
                self.strict.blank(self.text, token);
                if in_interface {
                    self.interface.blank(self.text, token);
                }
                if in_interface_strict {
                    self.interface_strict.blank(self.text, token);
                }
            }
            // The planted bug: a number's digits are in no code text.
            SyntaxKind::INT_NUMBER if planted(Plant::Stale) => {
                let range = token.text_range();
                let (start, end) = (usize::from(range.start()), usize::from(range.end()));
                self.inert.skip(self.text, start, end);
                self.strict.skip(self.text, start, end);
            }
            SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT => {
                if !inert {
                    self.inert.mask(self.text, token);
                    if in_interface {
                        self.interface.mask(self.text, token);
                    }
                }
                if !strict {
                    self.strict.mask(self.text, token);
                    if in_interface_strict {
                        self.interface_strict.mask(self.text, token);
                    }
                }
            }
            SyntaxKind::IDENT => match token.text() {
                "include_str" | "include_bytes" => {
                    self.whole |= !reads_other_than_rust(token);
                    self.reaches = true;
                }
                "include" => self.reaches = true,
                "path" => self.reaches |= names_path_attr(token),
                "blaze" | "fn_info" | "type_info" => self.extends = true,
                _ => {}
            },
            _ => {}
        }
    }
}

/// What the compiler reads in a comment's text: the bidirectional controls
/// (`text_direction_codepoint_in_comment`) and a carriage return (an error in a doc comment).
const READ_IN_COMMENTS: &[char] = &[
    '\r', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
    '\u{2068}', '\u{2069}',
];

/// Whether `token`, the name `path`, may be the `path` attribute: in an attribute, or, among
/// the tokens a macro is given, after a `[` or before a `=`.
fn names_path_attr(token: &SyntaxToken) -> bool {
    let Some(parent) = token.parent() else { return true };
    let mut given = false;
    for at in parent.ancestors() {
        match at.kind() {
            SyntaxKind::ATTR => return true,
            SyntaxKind::TOKEN_TREE => given = true,
            _ => {}
        }
    }
    let before = std::iter::successors(token.prev_token(), SyntaxToken::prev_token)
        .find(|it| !it.kind().is_trivia());
    let after = std::iter::successors(token.next_token(), SyntaxToken::next_token)
        .find(|it| !it.kind().is_trivia());
    given
        && (before.is_some_and(|it| it.kind() == SyntaxKind::L_BRACK)
            || after.is_some_and(|it| it.kind() == SyntaxKind::EQ))
}

/// Whether `node`, a macro call, is where items are: under nothing but modules, `impl`s and
/// traits.
fn at_items(node: &SyntaxNode) -> bool {
    node.ancestors().skip(1).all(|it| {
        matches!(
            it.kind(),
            SyntaxKind::SOURCE_FILE
                | SyntaxKind::ITEM_LIST
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::MODULE
                | SyntaxKind::IMPL
                | SyntaxKind::TRAIT
        )
    })
}

/// Whether the macro call `node` calls one of [`PLAIN_MACROS`] by its name alone (`std::vec!`
/// is not taken: `std` could be a module of the crate), given no tokens that could make an
/// item or call another macro.
fn plain_call(node: &SyntaxNode) -> bool {
    let named = node
        .children()
        .find(|it| it.kind() == SyntaxKind::PATH)
        .is_some_and(|path| PLAIN_MACROS.contains(&path_text(&path).as_str()));
    named
        && node.descendants_with_tokens().filter_map(NodeOrToken::into_token).all(|token| {
            match token.kind() {
                SyntaxKind::FN_KW
                | SyntaxKind::STRUCT_KW
                | SyntaxKind::ENUM_KW
                | SyntaxKind::UNION_KW
                | SyntaxKind::TRAIT_KW
                | SyntaxKind::IMPL_KW
                | SyntaxKind::CONST_KW
                | SyntaxKind::STATIC_KW
                | SyntaxKind::TYPE_KW
                | SyntaxKind::MOD_KW
                | SyntaxKind::USE_KW
                | SyntaxKind::EXTERN_KW
                | SyntaxKind::MACRO_KW
                | SyntaxKind::POUND => false,
                SyntaxKind::IDENT => !matches!(
                    token.text(),
                    "union" | "macro_rules" | "include" | "include_str" | "include_bytes"
                ),
                // The call's own `!` is after its own name, a plain macro's.
                SyntaxKind::BANG => !calls_other_macro(&token),
                _ => true,
            }
        })
}

/// A safeguard of the interface text a test takes away, to see its oracle find the edit the
/// safeguard is there for (`code/tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// A body with a type parameter is left out.
    Generic,
    /// A `const fn`'s body is left out.
    Const,
    /// An `#[inline]` body is left out.
    Inline,
    /// A body whose signature hides a type (`impl Trait`) is left out.
    Hidden,
    /// A trait's default method's body is left out.
    Trait,
    /// A body that defines an item is left out.
    Items,
    /// A body that calls a macro of unknown expansion is left out.
    Macros,
    /// A left-out body's shape holds no line count.
    Positions,
    /// A unit that only checks is keyed by the code of the crates it depends on, not their
    /// interfaces: no cutoff ([`crate::Narrowed::checked`]).
    Deep,
    /// A unit that builds code is keyed by the interfaces of the crates it depends on, not
    /// their code: a stale program ([`crate::Narrowed::code`]).
    Shallow,
    /// A file's texts hold its bytes, comments and whitespace as they are: a unit keyed by
    /// them is made again by an edit that moves no token and no position, no cutoff.
    Bytes,
    /// A number's digits are in no code text: a unit keyed by it is not made again when one
    /// changes, a stale unit.
    Stale,
}

#[cfg(test)]
thread_local! {
    /// The safeguard this thread's test took away.
    static PLANTED: std::cell::Cell<Option<Plant>> = const { std::cell::Cell::new(None) };
}

/// `run` with `plant` taken away, on this thread.
#[cfg(test)]
pub(crate) fn planting<T>(plant: Plant, run: impl FnOnce() -> T) -> T {
    PLANTED.with(|it| it.set(Some(plant)));
    let out = run();
    PLANTED.with(|it| it.set(None));
    out
}

/// Whether a test planted `plant`: never, outside this crate's tests.
#[cfg(test)]
pub(crate) fn planted(plant: Plant) -> bool {
    PLANTED.with(|it| it.get() == Some(plant))
}

#[cfg(not(test))]
pub(crate) fn planted(_plant: Plant) -> bool {
    false
}

/// Whether `body`, a block, is a function's body that no other crate's check can read, so the
/// crate's interface holds its shape in place of its text. A check of another crate reads a
/// body of this one only to: infer a type the signature hides (`impl Trait`, `async`, a
/// generator), evaluate it (`const`), instantiate it (a type or const parameter of the
/// function, its `impl` or its trait), inline it (`#[inline]`), or find an item it defines
/// (an `impl`, which is the crate's wherever it is written). So a body is left out only when:
///
/// - its function is written with nothing but attributes, a visibility, `unsafe`, an ABI,
///   lifetime parameters, parameters and a return type: no `const`, `async` or `default`, no
///   type or const parameter, no `where`, and no `impl Trait` or macro in its signature;
/// - no attribute of it names `inline` (one that gives it to a macro already keeps it: the
///   walk leaves no shape where a macro sees);
/// - it is under nothing but modules and `impl`s with no type or const parameter, no `where`,
///   no `const` and no `impl Trait` anywhere in them (an associated type that hides its type
///   takes it from the `impl`'s bodies): never in a trait (a default method is instantiated
///   for each `Self`), a function or a macro's tokens;
/// - it holds no item, no attribute that gives anything to a macro, and no macro call but a
///   [`plain_call`], which makes no item ([`CodeText::unplain`] says when one of those names
///   could mean another macro, and the crate then keeps every body).
///
/// What is proved of a body by blaze's extensions (effects, contracts) is read across crates
/// too: a crate that names them, or depends on one that does, takes no interface
/// ([`CodeText::extends`]). A code generator reads every body it reaches: a unit that builds
/// code takes no interface either.
pub(crate) fn plain_body(body: &SyntaxNode) -> bool {
    let Some(function) = body.parent().filter(|it| it.kind() == SyntaxKind::FN) else {
        return false;
    };
    let lifetimes_only =
        |list: &SyntaxNode| list.children().all(|it| it.kind() == SyntaxKind::LIFETIME_PARAM);
    let signature = function.children_with_tokens().all(|element| match element {
        NodeOrToken::Token(token) => matches!(
            token.kind(),
            SyntaxKind::WHITESPACE
                | SyntaxKind::COMMENT
                | SyntaxKind::OUTER_DOC_COMMENT
                | SyntaxKind::INNER_DOC_COMMENT
                | SyntaxKind::FN_KW
                | SyntaxKind::UNSAFE_KW
                | SyntaxKind::SAFE_KW
        ) || (token.kind() == SyntaxKind::CONST_KW && planted(Plant::Const)),
        NodeOrToken::Node(node) => match node.kind() {
            SyntaxKind::ATTR => {
                planted(Plant::Inline)
                    || !node
                        .descendants_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .any(|it| it.kind() == SyntaxKind::IDENT && it.text() == "inline")
            }
            SyntaxKind::VISIBILITY | SyntaxKind::ABI | SyntaxKind::NAME => true,
            SyntaxKind::GENERIC_PARAM_LIST => lifetimes_only(&node) || planted(Plant::Generic),
            SyntaxKind::PARAM_LIST | SyntaxKind::RET_TYPE => {
                planted(Plant::Hidden)
                    || !node.descendants().any(|it| {
                        matches!(it.kind(), SyntaxKind::IMPL_TRAIT_TYPE | SyntaxKind::MACRO_CALL)
                    })
            }
            SyntaxKind::BLOCK_EXPR => true,
            _ => false,
        },
    });
    if !signature {
        return false;
    }
    let placed = function.ancestors().skip(1).all(|at| match at.kind() {
        SyntaxKind::SOURCE_FILE
        | SyntaxKind::ITEM_LIST
        | SyntaxKind::ASSOC_ITEM_LIST
        | SyntaxKind::MODULE => true,
        SyntaxKind::IMPL => {
            at.children_with_tokens().all(|element| match element {
                NodeOrToken::Token(token) => token.kind() != SyntaxKind::CONST_KW,
                NodeOrToken::Node(node) => match node.kind() {
                    SyntaxKind::WHERE_CLAUSE => false,
                    SyntaxKind::GENERIC_PARAM_LIST => lifetimes_only(&node),
                    _ => true,
                },
            // An associated type that hides its type (`type Item = impl Trait;`) is given it
            // by the bodies of its `impl`'s functions, whatever their signatures say.
            }) && !at.descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
        }
        SyntaxKind::TRAIT => planted(Plant::Trait),
        _ => false,
    });
    placed
        && body.descendants().all(|node| match node.kind() {
            SyntaxKind::MACRO_CALL => plain_call(&node) || planted(Plant::Macros),
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => false,
            _ => match ast::Attr::cast(node.clone()) {
                Some(attr) => attr_seen(&attr) == Seen::No,
                None => ast::Item::cast(node).is_none() || planted(Plant::Items),
            },
        })
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
    use_takes(node, |it| SHADOWABLE.contains(&it))
}

/// Whether the `use` item `node` brings in a name `named` takes, or a glob, from a path that
/// starts outside the standard library and the crate.
fn use_takes(node: &SyntaxNode, named: impl Fn(&str) -> bool) -> bool {
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
        from_outside && (glob || name.is_some_and(|it| named(it.as_str())))
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
