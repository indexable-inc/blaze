//! The walk that makes an [`Outline`]: the parser's event stream for the whole file, read once
//! with a stack of the nodes open at each step and, beside it, the entries open at each step.
//!
//! Each raw token the parser takes is found in the lexer's tokens (trivia skipped), given to
//! every open node that has no start yet, and fed to the hashers of every open entry. A node
//! the outline reads is recognised by its kind and its parent's role ([`Role`]); what it holds
//! is taken when it closes, from its range or from the tokens recorded while it was open (a
//! visibility, a rename, an attribute). No node is kept past its exit.

use std::alloc::Allocator;

use blaze_hash::ByHash;
use blaze_ids::{ContentId, Hasher};
use parser::{
    Edition, LexedStr, OutlineStep, Step,
    SyntaxKind::{self, *},
};

use crate::{
    Attr, AttrKind, AttrSpan, ErrorOrigin, ITEM_DOMAIN, Item, ItemIndex, ItemKind, Outline, Range,
    Rename, SIGNATURE_DOMAIN, SyntaxError, Visibility, usize_of,
};

/// The domain of an unnamed entry's head: its signature tokens outside its attributes, which
/// stand for its name among its siblings.
const HEAD_DOMAIN: &str = "blaze v2 file item head 1";

/// The domain of a sibling group's key: the parent, the kind and the name (or head).
const SIBLING_DOMAIN: &str = "blaze v2 file item siblings 1";

/// What an entry is before the walk closes it.
const PLACEHOLDER: Item = Item {
    kind: ItemKind::Error,
    parent: None,
    end: ItemIndex(0),
    range: Range::new(0, 0),
    name: None,
    visibility: Visibility::Private,
    signature: Range::new(0, 0),
    body: None,
    attrs: AttrSpan { start: 0, end: 0 },
    disambiguator: 0,
    hash: ContentId([0; 32]),
    signature_hash: ContentId([0; 32]),
};

pub(crate) fn outline<A: Allocator + Clone>(text: &str, edition: Edition, alloc: A) -> Outline<A> {
    assert!(
        u32::try_from(text.len()).is_ok(),
        "a source file of {} bytes: an outline's ranges are u32 pairs",
        text.len()
    );
    let lexed = LexedStr::new(edition, text);
    let input = lexed.to_input(edition);
    let output = parser::TopEntryPoint::SourceFile.parse(input.tokens());
    let mut walk = Walk::new(text, &lexed, edition, alloc);
    for step in output.steps() {
        match step {
            OutlineStep::Step(Step::Token { n_input_tokens, .. }) => {
                for _ in 0..n_input_tokens {
                    walk.token();
                }
            }
            // `x.0.1`: one float token, then the node it ends and, past a second field, the
            // one the parser left open for it (`Output`'s depth accounting).
            OutlineStep::Step(Step::FloatSplit { ends_in_dot }) => {
                walk.token();
                walk.exit();
                if !ends_in_dot {
                    walk.exit();
                }
            }
            OutlineStep::Step(Step::Enter { kind }) => walk.enter(kind),
            OutlineStep::Step(Step::Exit) => walk.exit(),
            OutlineStep::Step(Step::Error { msg }) => walk.parser_error(msg),
            OutlineStep::Lazy { kind, .. } => {
                unreachable!("a full parse's output holds a lazy {kind:?}")
            }
        }
    }
    walk.finish()
}

/// Whose a list or an attribute is: the file's, or an entry's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    File,
    Item(ItemIndex),
}

/// What a node is to the outline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Nothing the outline reads by itself.
    Plain,
    /// An entry: its [`Open`] is the top of [`Walk::open`] while it is open.
    Entry,
    /// A node whose node children are entries of the owner: the file, or an entry's body.
    List(Owner),
    /// An entry's body that is not a list: a block, a token tree, a value.
    Body,
    /// An entry's visibility or rename, read from its recorded tokens.
    Decode,
    /// An attribute or doc comment of the owner, read from its recorded tokens.
    Attr(Owner),
}

/// An open node.
struct Frame {
    kind: SyntaxKind,
    /// Where its first token starts, once it has one.
    start: Option<u32>,
    role: Role,
    /// It is within the body of a `fn`, `const` or `static` (the top open entry).
    in_body: bool,
}

/// A token recorded while a [`Role::Decode`] or [`Role::Attr`] node is open.
#[derive(Clone, Copy)]
struct Tok {
    kind: SyntaxKind,
    range: Range,
}

/// An open entry. Its ranges are the file's until it closes.
struct Open {
    index: ItemIndex,
    parent: Option<ItemIndex>,
    syntax: SyntaxKind,
    whole: Hasher,
    signature: Hasher,
    /// For an entry with no name: its signature tokens outside its attributes.
    head: Option<Hasher>,
    head_fed: bool,
    start: Option<u32>,
    /// The end of its last token so far.
    last_end: Option<u32>,
    body_open: bool,
    /// One of its own attributes is open.
    attr_open: bool,
    /// Where its tokens before the body, a direct `=` or a direct `;` end.
    before_body: Option<u32>,
    before_eq: Option<u32>,
    before_semi: Option<u32>,
    after_colon: bool,
    after_eq: bool,
    name: Option<Range>,
    visibility: Visibility,
    rename: Option<Rename>,
    tree: Option<Range>,
    path: Option<Range>,
    input: Option<Range>,
    abi: Option<Range>,
    ty: Option<Range>,
    /// A variant's discriminant or a field's default.
    value: Option<Range>,
    body: Option<Range>,
    holds_items: bool,
    attrs: AttrSpan,
    children: u32,
}

struct Walk<'t, 'l, A: Allocator + Clone> {
    text: &'t str,
    lexed: &'l LexedStr<'t>,
    edition: u8,
    /// The next lexer token.
    pos: usize,
    /// Where token `pos` starts, or the trivia before it.
    offset: u32,
    /// The end of the last token the parser took.
    prev_end: Option<u32>,
    frames: Vec<Frame, A>,
    open: Vec<Open, A>,
    /// The depth of the node whose tokens are being recorded into `tokens`.
    recorded: Option<usize>,
    tokens: Vec<Tok, A>,
    /// How many entries of each sibling key the walk has closed.
    siblings: ByHash<ContentId, u32, A>,
    file_children: u32,
    out: Outline<A>,
}

impl<'t, 'l, A: Allocator + Clone> Walk<'t, 'l, A> {
    fn new(text: &'t str, lexed: &'l LexedStr<'t>, edition: Edition, alloc: A) -> Self {
        Walk {
            text,
            lexed,
            edition: edition_byte(edition),
            pos: 0,
            offset: 0,
            prev_end: None,
            frames: Vec::new_in(alloc.clone()),
            open: Vec::new_in(alloc.clone()),
            recorded: None,
            tokens: Vec::new_in(alloc.clone()),
            siblings: ByHash::new_in(alloc.clone()),
            file_children: 0,
            out: Outline {
                items: Vec::new_in(alloc.clone()),
                attrs: Vec::new_in(alloc.clone()),
                file_attrs: AttrSpan::default(),
                errors: Vec::new_in(alloc.clone()),
                messages: Vec::new_in(alloc),
            },
        }
    }

    fn token_len(&self, at: usize) -> u32 {
        u32::try_from(self.lexed.text_len(at)).expect("a token of a file under 4 GiB")
    }

    fn skip_trivia(&mut self) {
        while self.pos < self.lexed.len() && self.lexed.kind(self.pos).is_trivia() {
            self.offset += self.token_len(self.pos);
            self.pos += 1;
        }
    }

    /// The kind of the `skip`-th token from `pos` that is not trivia.
    fn peek(&self, skip: usize) -> Option<SyntaxKind> {
        let mut seen = 0;
        for at in self.pos..self.lexed.len() {
            let kind = self.lexed.kind(at);
            if kind.is_trivia() {
                continue;
            }
            if seen == skip {
                return Some(kind);
            }
            seen += 1;
        }
        None
    }

    /// Whether the attribute or doc comment starting at `pos` is an inner one.
    fn next_is_inner(&self) -> bool {
        match self.peek(0) {
            Some(INNER_DOC_COMMENT) => true,
            Some(POUND) => self.peek(1) == Some(BANG),
            _ => false,
        }
    }

    /// The parser took one raw token.
    fn token(&mut self) {
        self.skip_trivia();
        assert!(self.pos < self.lexed.len(), "the parser took a token past the lexer's last");
        let kind = self.lexed.kind(self.pos);
        let range = Range::new(self.offset, self.offset + self.token_len(self.pos));
        let touches = self.prev_end == Some(range.start);
        for frame in self.frames.iter_mut().rev() {
            if frame.start.is_some() {
                break;
            }
            frame.start = Some(range.start);
        }
        if self.frames.last().is_some_and(|frame| frame.role == Role::Entry) {
            let open = self.open.last_mut().expect("an entry's node has its open entry");
            let before = open.last_end.unwrap_or(range.start);
            match kind {
                SEMICOLON => open.before_semi = Some(before),
                EQ => {
                    open.before_eq.get_or_insert(before);
                    open.after_eq = true;
                }
                COLON => open.after_colon = true,
                _ => {}
            }
        }
        if self.recorded.is_some() {
            self.tokens.push(Tok { kind, range });
        }
        let bytes = range.of(self.text).as_bytes();
        for open in self.open.iter_mut() {
            // Whether it touches the token before it in this entry: never for its first.
            let joint = [u8::from(touches && open.last_end.is_some())];
            open.whole.update(&joint).part(bytes);
            if !open.body_open {
                open.signature.update(&joint).part(bytes);
                if !open.attr_open
                    && let Some(head) = &mut open.head
                {
                    head.update(&[u8::from(touches && open.head_fed)]).part(bytes);
                    open.head_fed = true;
                }
            }
            open.start.get_or_insert(range.start);
            open.last_end = Some(range.end);
        }
        self.prev_end = Some(range.end);
        self.offset = range.end;
        self.pos += 1;
    }

    fn enter(&mut self, kind: SyntaxKind) {
        let Some(parent) = self.frames.last() else {
            assert_eq!(kind, SOURCE_FILE, "a source file's parse starts with its root");
            self.frames.push(Frame {
                kind,
                start: None,
                role: Role::List(Owner::File),
                in_body: false,
            });
            return;
        };
        let (parent_kind, parent_role, in_body) = (parent.kind, parent.role, parent.in_body);
        if in_body && is_item(kind) {
            // A node in a body is under the body's entry, the top open one: none opens inside.
            self.open.last_mut().expect("a body's entry is open").holds_items = true;
        }
        let (role, opens_body) = match parent_role {
            Role::List(owner) => (self.list_child(kind, parent_kind, owner), false),
            Role::Entry => self.entry_child(kind),
            Role::Plain | Role::Body | Role::Decode | Role::Attr(_) => (Role::Plain, false),
        };
        if matches!(role, Role::Decode | Role::Attr(_)) {
            self.recorded = Some(self.frames.len());
            self.tokens.clear();
        }
        self.frames.push(Frame { kind, start: None, role, in_body: in_body || opens_body });
    }

    /// A node entered in a list: an attribute of the list's owner, or an entry.
    fn list_child(&mut self, kind: SyntaxKind, list: SyntaxKind, owner: Owner) -> Role {
        let children = match owner {
            Owner::File => self.file_children,
            Owner::Item(_) => self.open.last().expect("a list's owner is open").children,
        };
        // An inner attribute before the first item is the owner's; any other at item level
        // (an outer one with no item, an inner one after an item) is an error entry.
        if matches!(kind, ATTR | DOC_COMMENT)
            && is_item_list(list)
            && children == 0
            && self.next_is_inner()
        {
            return Role::Attr(owner);
        }
        self.open_entry(kind, owner);
        Role::Entry
    }

    fn open_entry(&mut self, syntax: SyntaxKind, owner: Owner) {
        let index = ItemIndex(
            u32::try_from(self.out.items.len()).expect("an outline's entries are counted in u32"),
        );
        self.out.items.push(PLACEHOLDER);
        let parent = match owner {
            Owner::File => {
                self.file_children += 1;
                None
            }
            Owner::Item(parent) => {
                self.open.last_mut().expect("a list's owner is open").children += 1;
                Some(parent)
            }
        };
        let at = u32::try_from(self.out.attrs.len()).expect("attributes are counted in u32");
        let mut whole = Hasher::new(ITEM_DOMAIN);
        whole.update(&[self.edition]);
        let mut signature = Hasher::new(SIGNATURE_DOMAIN);
        signature.update(&[self.edition]);
        self.open.push(Open {
            index,
            parent,
            syntax,
            whole,
            signature,
            head: (!is_named(syntax)).then(|| Hasher::new(HEAD_DOMAIN)),
            head_fed: false,
            start: None,
            last_end: None,
            body_open: false,
            attr_open: false,
            before_body: None,
            before_eq: None,
            before_semi: None,
            after_colon: false,
            after_eq: false,
            name: None,
            visibility: Visibility::Private,
            rename: None,
            tree: None,
            path: None,
            input: None,
            abi: None,
            ty: None,
            value: None,
            body: None,
            holds_items: false,
            attrs: AttrSpan { start: at, end: at },
            children: 0,
        });
    }

    /// A node entered directly in the top open entry, and whether it opens a body whose
    /// items and macro calls the entry is to say it holds.
    fn entry_child(&mut self, kind: SyntaxKind) -> (Role, bool) {
        let next = self.peek(0);
        let here = self.offset;
        let open = self.open.last_mut().expect("an entry's node has its open entry");
        match kind {
            ATTR | DOC_COMMENT => {
                open.attr_open = true;
                return (Role::Attr(Owner::Item(open.index)), false);
            }
            VISIBILITY | RENAME => return (Role::Decode, false),
            _ => {}
        }
        let starts_body = open.body.is_none()
            && !open.body_open
            && match open.syntax {
                FN => kind == BLOCK_EXPR,
                MODULE => kind == ITEM_LIST,
                TRAIT | IMPL => kind == ASSOC_ITEM_LIST,
                EXTERN_BLOCK => kind == EXTERN_ITEM_LIST,
                ENUM => kind == VARIANT_LIST,
                STRUCT | UNION | VARIANT => matches!(kind, RECORD_FIELD_LIST | TUPLE_FIELD_LIST),
                MACRO_RULES | MACRO_CALL => kind == TOKEN_TREE,
                // `macro m($x:expr) { .. }`: the arguments are a `( .. )`, the body a `{ .. }`.
                MACRO_DEF => kind == TOKEN_TREE && next == Some(L_CURLY),
                CONST | STATIC => open.after_eq,
                _ => false,
            };
        if !starts_body {
            return (Role::Plain, false);
        }
        open.body_open = true;
        open.before_body = Some(open.last_end.unwrap_or(here));
        let role = match is_list(kind) {
            true => Role::List(Owner::Item(open.index)),
            false => Role::Body,
        };
        (role, matches!(open.syntax, FN | CONST | STATIC))
    }

    fn exit(&mut self) {
        let frame = self.frames.pop().expect("the parser's exits match its enters");
        let range = match frame.start {
            Some(start) => {
                Range::new(start, self.prev_end.expect("a node with a token has its token's end"))
            }
            None => Range::new(self.offset, self.offset),
        };
        let recorded = self.recorded == Some(self.frames.len());
        match frame.role {
            Role::Entry => self.close_entry(range),
            Role::Attr(owner) => self.close_attr(owner, frame.kind, range),
            Role::Decode => self.decode(frame.kind),
            Role::List(_) | Role::Body | Role::Plain => {}
        }
        if self.frames.last().is_some_and(|parent| parent.role == Role::Entry) {
            self.direct_child(frame.kind, frame.role, range);
        }
        if recorded {
            self.recorded = None;
            self.tokens.clear();
        }
    }

    /// A node directly in the top open entry closed over `range`.
    fn direct_child(&mut self, kind: SyntaxKind, role: Role, range: Range) {
        let open = self.open.last_mut().expect("an entry's node has its open entry");
        // A node with no token before the entry's first: the entry starts where it does.
        open.start.get_or_insert(range.start);
        match role {
            Role::Body | Role::List(_) => {
                open.body = Some(range);
                open.body_open = false;
                if open.syntax == MACRO_CALL {
                    open.input = Some(range);
                }
                return;
            }
            Role::Attr(_) | Role::Decode | Role::Entry => return,
            Role::Plain => {}
        }
        match (open.syntax, kind) {
            (_, NAME) => {
                open.name.get_or_insert(range);
            }
            (EXTERN_CRATE, NAME_REF) => {
                open.name.get_or_insert(range);
            }
            (USE, USE_TREE) => open.tree = Some(range),
            (MACRO_CALL, PATH) => open.path = Some(range),
            (EXTERN_BLOCK, ABI) => open.abi = Some(range),
            (RECORD_FIELD, _) if open.after_colon && open.ty.is_none() => open.ty = Some(range),
            (TUPLE_FIELD, _) if kind != MUT_RESTRICTION && open.ty.is_none() => {
                open.ty = Some(range)
            }
            (VARIANT | RECORD_FIELD, _) if open.after_eq && open.value.is_none() => {
                open.value = Some(range)
            }
            _ => {}
        }
    }

    /// A visibility or rename of the top open entry closed: read it from its tokens.
    fn decode(&mut self, kind: SyntaxKind) {
        let tokens = &self.tokens;
        let open = self.open.last_mut().expect("an entry's node has its open entry");
        match kind {
            VISIBILITY => open.visibility = visibility_of(tokens),
            RENAME => open.rename = rename_of(tokens),
            _ => unreachable!("only a visibility or a rename is decoded, not a {kind:?}"),
        }
    }

    fn close_attr(&mut self, owner: Owner, kind: SyntaxKind, range: Range) {
        let (inner, attr) = attr_of(kind, &self.tokens, self.text);
        let base = match owner {
            Owner::File => 0,
            Owner::Item(_) => self
                .open
                .last()
                .expect("an attribute's owner is open")
                .start
                .expect("an attribute's first token started its owner"),
        };
        let attr = match attr {
            AttrKind::Path { literal } => AttrKind::Path { literal: relative(literal, base) },
            other => other,
        };
        self.out.attrs.push(Attr { range: relative(range, base), inner, kind: attr });
        let end = u32::try_from(self.out.attrs.len()).expect("attributes are counted in u32");
        match owner {
            Owner::File => self.out.file_attrs.end = end,
            Owner::Item(_) => {
                let open = self.open.last_mut().expect("an attribute's owner is open");
                open.attrs.end = end;
                open.attr_open = false;
            }
        }
    }

    fn close_entry(&mut self, range: Range) {
        let open = self.open.pop().expect("an entry's node has its open entry");
        let start = open.start.unwrap_or(range.start);
        let end = range.end.max(start);
        let rel = |part: Range| relative(part, start);
        let signature_end = match open.syntax {
            CONST | STATIC => open.before_eq.or(open.before_body),
            _ => open.before_body,
        }
        .or(open.before_semi)
        .unwrap_or(end)
        .max(start);
        let attrs = &self.out.attrs[usize_of(open.attrs.start)..usize_of(open.attrs.end)];
        let kind = match open.syntax {
            FN => ItemKind::Fn { body_holds_items: open.holds_items },
            STRUCT => ItemKind::Struct,
            ENUM => ItemKind::Enum,
            UNION => ItemKind::Union,
            TRAIT if open.before_eq.is_some() => ItemKind::TraitAlias,
            TRAIT => ItemKind::Trait,
            IMPL => ItemKind::Impl,
            CONST => ItemKind::Const { body_holds_items: open.holds_items },
            STATIC => ItemKind::Static { body_holds_items: open.holds_items },
            TYPE_ALIAS => ItemKind::TypeAlias,
            MODULE => ItemKind::Mod {
                inline: open.body.is_some(),
                path: attrs.iter().find_map(|attr| match attr.kind {
                    AttrKind::Path { literal } if !attr.inner => Some(literal),
                    _ => None,
                }),
            },
            USE => ItemKind::Use { tree: open.tree.map(rel) },
            EXTERN_CRATE => ItemKind::ExternCrate {
                rename: open.rename.map(|rename| match rename {
                    Rename::Name(name) => Rename::Name(rel(name)),
                    Rename::Underscore => Rename::Underscore,
                }),
            },
            EXTERN_BLOCK => ItemKind::ExternBlock { abi: open.abi.map(rel) },
            MACRO_RULES => ItemKind::MacroRules {
                exported: attrs.iter().any(|attr| !attr.inner && attr.kind == AttrKind::MacroExport),
            },
            MACRO_DEF => ItemKind::MacroDef,
            MACRO_CALL => {
                ItemKind::MacroCall { path: open.path.map(rel), input: open.input.map(rel) }
            }
            VARIANT => ItemKind::Variant { discriminant: open.value.map(rel) },
            RECORD_FIELD | TUPLE_FIELD => {
                ItemKind::Field { ty: open.ty.map(rel), default: open.value.map(rel) }
            }
            ERROR | ATTR | DOC_COMMENT => ItemKind::Error,
            syntax => ItemKind::Unhandled { syntax },
        };
        let mut sibling = Hasher::new(SIBLING_DOMAIN);
        sibling
            .update(&open.parent.map_or(0u64, |parent| u64::from(parent.0) + 1).to_le_bytes())
            .update(&[tag_of(kind)]);
        match (&open.head, open.syntax) {
            (Some(head), _) => {
                sibling.id(&head.finish());
            }
            // A tuple field is told apart by its position alone.
            (None, TUPLE_FIELD) => {}
            (None, _) => {
                sibling.opt_part(open.name.map(|name| {
                    let name = name.of(self.text);
                    name.strip_prefix("r#").unwrap_or(name)
                }));
            }
        }
        let count = self.siblings.entry(sibling.finish()).or_insert(0);
        let disambiguator = *count;
        *count += 1;
        let after = u32::try_from(self.out.items.len()).expect("an outline's entries are counted in u32");
        self.out.items[open.index.at()] = Item {
            kind,
            parent: open.parent,
            end: ItemIndex(after),
            range: Range::new(start, end),
            name: open.name.map(rel),
            visibility: match open.visibility {
                Visibility::In { path } => Visibility::In { path: rel(path) },
                Visibility::WithoutIn { path } => Visibility::WithoutIn { path: rel(path) },
                other => other,
            },
            signature: Range::new(0, signature_end - start),
            body: open.body.map(rel),
            attrs: open.attrs,
            disambiguator,
            hash: open.whole.finish(),
            signature_hash: open.signature.finish(),
        };
    }

    fn parser_error(&mut self, message: &str) {
        let range = Range::new(self.offset, self.offset);
        self.push_error(range, ErrorOrigin::Parser, message);
    }

    fn push_error(&mut self, range: Range, origin: ErrorOrigin, message: &str) {
        let start = u32::try_from(self.out.messages.len()).expect("messages under 4 GiB");
        self.out.messages.extend_from_slice(message.as_bytes());
        let end = u32::try_from(self.out.messages.len()).expect("messages under 4 GiB");
        self.out.errors.push(SyntaxError { range, origin, message: Range::new(start, end) });
    }

    fn finish(mut self) -> Outline<A> {
        assert!(
            self.frames.is_empty() && self.open.is_empty(),
            "the parser's output closes every node it opens"
        );
        let lexed = self.lexed;
        for (token, message) in lexed.errors() {
            let at = lexed.text_range(token);
            let range = Range::new(
                u32::try_from(at.start).expect("a file under 4 GiB"),
                u32::try_from(at.end).expect("a file under 4 GiB"),
            );
            self.push_error(range, ErrorOrigin::Lexer, message);
        }
        self.out
    }
}

/// `part`, a range of the file inside an item starting at `base`, relative to the item.
fn relative(part: Range, base: u32) -> Range {
    let at = |offset: u32| offset.checked_sub(base).expect("a part of an item is inside it");
    Range::new(at(part.start), at(part.end))
}

/// The edition as one byte of a hash, by name, so a reordering of `Edition` changes no hash.
fn edition_byte(edition: Edition) -> u8 {
    match edition {
        Edition::Edition2015 => 0,
        Edition::Edition2018 => 1,
        Edition::Edition2021 => 2,
        Edition::Edition2024 => 3,
    }
}

/// The kinds of node an item list holds as items (and a body holds as items or macro calls).
fn is_item(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        CONST
            | ENUM
            | EXTERN_BLOCK
            | EXTERN_CRATE
            | FN
            | IMPL
            | MACRO_CALL
            | MACRO_RULES
            | MACRO_DEF
            | MODULE
            | STATIC
            | STRUCT
            | TRAIT
            | TYPE_ALIAS
            | UNION
            | USE
    )
}

/// The kinds of entry told apart from their siblings by name: the rest, by their head.
fn is_named(syntax: SyntaxKind) -> bool {
    matches!(
        syntax,
        FN | STRUCT
            | ENUM
            | UNION
            | TRAIT
            | CONST
            | STATIC
            | TYPE_ALIAS
            | MODULE
            | EXTERN_CRATE
            | MACRO_RULES
            | MACRO_DEF
            | VARIANT
            | RECORD_FIELD
            | TUPLE_FIELD
    )
}

/// The lists whose children are items, and may hold their owner's inner attributes.
fn is_item_list(kind: SyntaxKind) -> bool {
    matches!(kind, SOURCE_FILE | ITEM_LIST | ASSOC_ITEM_LIST | EXTERN_ITEM_LIST)
}

/// The bodies whose node children are entries.
fn is_list(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        ITEM_LIST
            | ASSOC_ITEM_LIST
            | EXTERN_ITEM_LIST
            | VARIANT_LIST
            | RECORD_FIELD_LIST
            | TUPLE_FIELD_LIST
    )
}

/// The kind of an entry as one byte of its sibling key: its variant, not its payload.
fn tag_of(kind: ItemKind) -> u8 {
    match kind {
        ItemKind::Fn { .. } => 0,
        ItemKind::Struct => 1,
        ItemKind::Enum => 2,
        ItemKind::Union => 3,
        ItemKind::Trait => 4,
        ItemKind::TraitAlias => 5,
        ItemKind::Impl => 6,
        ItemKind::Const { .. } => 7,
        ItemKind::Static { .. } => 8,
        ItemKind::TypeAlias => 9,
        ItemKind::Mod { .. } => 10,
        ItemKind::Use { .. } => 11,
        ItemKind::ExternCrate { .. } => 12,
        ItemKind::ExternBlock { .. } => 13,
        ItemKind::MacroRules { .. } => 14,
        ItemKind::MacroDef => 15,
        ItemKind::MacroCall { .. } => 16,
        ItemKind::Variant { .. } => 17,
        ItemKind::Field { .. } => 18,
        ItemKind::Error => 19,
        ItemKind::Unhandled { .. } => 20,
    }
}

/// A visibility from its tokens: `pub`, then `( .. )` or nothing. Ranges are the file's.
fn visibility_of(tokens: &[Tok]) -> Visibility {
    let Some(paren) = tokens.get(1).filter(|token| token.kind == L_PAREN) else {
        return Visibility::Pub;
    };
    let inside = match tokens {
        [_, _, inside @ .., close] if close.kind == R_PAREN => inside,
        [_, _, inside @ ..] => inside,
        _ => &[],
    };
    let span = |path: &[Tok]| match (path.first(), path.last()) {
        (Some(first), Some(last)) => Range::new(first.range.start, last.range.end),
        _ => Range::new(paren.range.end, paren.range.end),
    };
    match inside {
        [only] if only.kind == CRATE_KW => Visibility::Crate,
        [only] if only.kind == SUPER_KW => Visibility::Super,
        [only] if only.kind == SELF_KW => Visibility::SelfOnly,
        [first, path @ ..] if first.kind == IN_KW => Visibility::In { path: span(path) },
        path => Visibility::WithoutIn { path: span(path) },
    }
}

/// An `extern crate`'s rename from its tokens: `as`, then a name or `_`.
fn rename_of(tokens: &[Tok]) -> Option<Rename> {
    match tokens.get(1) {
        Some(token) if token.kind == UNDERSCORE => Some(Rename::Underscore),
        Some(token) => Some(Rename::Name(token.range)),
        None => None,
    }
}

/// Whether an attribute (`#[..]`, `#![..]`) or doc comment is inner, and what it is, from its
/// tokens; a `#[path]` literal's range is the file's.
fn attr_of(kind: SyntaxKind, tokens: &[Tok], text: &str) -> (bool, AttrKind) {
    if kind == DOC_COMMENT {
        return (tokens.first().is_some_and(|token| token.kind == INNER_DOC_COMMENT), AttrKind::Doc);
    }
    let is = |at: usize, kind: SyntaxKind| tokens.get(at).is_some_and(|token| token.kind == kind);
    let inner = is(1, BANG);
    // Past `#`, `!` and `[`, and an `unsafe(` around the meta.
    let mut at = if inner { 3 } else { 2 };
    if is(at, UNSAFE_KW) && is(at + 1, L_PAREN) {
        at += 2;
    }
    // A path of one segment: `a::b` names none of these.
    let Some(first) = tokens
        .get(at)
        .filter(|token| token.kind == IDENT && !is(at + 1, COLON) && !is(at + 1, COLON2))
    else {
        return (inner, AttrKind::Other);
    };
    let attr = match first.range.of(text) {
        "cfg" => AttrKind::Cfg,
        "cfg_attr" => AttrKind::CfgAttr,
        "doc" => AttrKind::Doc,
        "macro_export" => AttrKind::MacroExport,
        "path" if is(at + 1, EQ) && is(at + 2, STRING) => {
            AttrKind::Path { literal: tokens[at + 2].range }
        }
        _ => AttrKind::Other,
    };
    (inner, attr)
}
