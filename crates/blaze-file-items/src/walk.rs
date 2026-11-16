//! The walk that makes an [`Outline`]: the parser's event stream for the whole file, read once
//! with a stack of the nodes open at each step and, beside it, the entries open at each step.
//!
//! Each raw token the parser takes is found in the lexer's tokens (trivia skipped) and given to
//! every open node that has no start yet; an entry's digests are taken from its bytes when it
//! closes. A node
//! the outline reads is recognised by its kind and its parent's role ([`Role`]); what it holds
//! is taken when it closes, from its range or from the tokens recorded while it was open (a
//! visibility, a rename, an attribute). No node is kept past its exit.

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    Attr, AttrKind, AttrSpan, ErrorOrigin, Item, ItemDetail, ItemFlags, ItemIndex, Range, Rename,
    SyntaxError, WrittenVis,
};
use parser::{
    Edition, Input, LexedStr, OutlineStep, Reparser, Step,
    SyntaxKind::{self, *},
};

use blaze_lex::Cpu;

use crate::flags::Scan;
use crate::skeleton::{Skipped, skeleton};
use crate::Outline;
use crate::digest::{By, Compress, Keyed, digest};

/// What an entry is before the walk closes it.
const PLACEHOLDER: Item = Item {
    detail: ItemDetail::Error,
    parent: None,
    end: ItemIndex(0),
    range: Range::new(0, 0),
    name: None,
    visibility: WrittenVis::Private,
    signature: Range::new(0, 0),
    body: None,
    attrs: AttrSpan { start: 0, end: 0 },
    disambiguator: 0,
    hash: ContentId([0; 32]),
    signature_hash: ContentId([0; 32]),
    flags: ItemFlags::NONE,
};

/// The outline of `text`, walked whole.
pub(crate) fn outline<'a, C: Compress>(
    text: &str,
    edition: Edition,
    bodies: Bodies,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    let whole = Range::new(0, u32::try_from(text.len()).unwrap_or(u32::MAX));
    outline_parts::<C>(text, [whole], edition, bodies, alloc)
}

/// The outline of `text` walked part by part, `parts` in order (crate::split_top_level's, or
/// the whole text as one): each part's walk appends its entries, attributes and errors to the
/// file's one outline, its ranges moved by the part's start as they are written, so no part
/// has an outline of its own and nothing is copied to put them together. Then the digests of
/// every entry, in fixed batches (crate::digest).
///
/// # Panics
///
/// When a part but the first holds file attributes or file flags, which `split_top_level`
/// never cuts for: a file's own attribute is before its first item.
pub(crate) fn outline_parts<'a, C: Compress>(
    text: &str,
    parts: impl IntoIterator<Item = Range>,
    edition: Edition,
    bodies: Bodies,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    assert!(
        u32::try_from(text.len()).is_ok(),
        "a source file of {} bytes: an outline's ranges are u32 pairs",
        text.len()
    );
    let mut buffers = blaze_mem::scratch::lend::<Buffers>();
    let Buffers { out, working } = &mut *buffers;
    for (index, part) in parts.into_iter().enumerate() {
        let own = (out.file_attrs, out.own_flags);
        walk_part(text, part, edition, bodies, working, out);
        assert!(
            index == 0 || (out.file_attrs, out.own_flags) == own,
            "a part but the first holds file attributes or file flags"
        );
    }
    // The lexer's errors come after every part's parser errors, as a whole-file walk has them.
    let Lexer { errors, messages } = &working.lexer;
    let shift = u32::try_from(out.messages.len()).expect("messages under 4 GiB");
    out.messages.extend_from_slice(messages);
    out.errors.extend(errors.iter().map(|error| SyntaxError { message: error.message.shifted(shift), ..*error }));
    out.flags = out.items.iter().fold(out.own_flags, |flags, item| flags | item.flags);
    digest::<C>(text, edition_byte(edition), out, &mut working.digest);
    out.copy_in(alloc)
}

/// What [`outline_parts`] grows and no caller keeps, one set per worker at the most it ever
/// held (`blaze_mem::scratch`, docs/v2/plan/design/memory.md, "Scratch buffers reused per
/// worker"), like the parser's events: the outline as the walk appends to it and the walk's
/// working vectors. In a bump frame two vectors that grow by turns copy each other at every
/// step, and a working vector is never freed while a kept one is above it, so the frame would
/// hold twice the outline and the working set beside it; here the frame takes the outline
/// once, at its exact size ([`Outline::copy_in`]), and the worker's set stops growing once
/// warm. The heap is `Alloc::global`'s, named here and nowhere else in the crate.
struct Buffers {
    out: Outline<'static>,
    working: Working<'static>,
}

impl Default for Buffers {
    fn default() -> Buffers {
        let alloc = Alloc::global();
        Buffers {
            out: Outline {
                items: Vec::new_in(alloc),
                attrs: Vec::new_in(alloc),
                file_attrs: AttrSpan::default(),
                errors: Vec::new_in(alloc),
                messages: Vec::new_in(alloc),
                keys: Vec::new_in(alloc),
                own_flags: ItemFlags::NONE,
                flags: ItemFlags::NONE,
            },
            working: Working::new_in(alloc),
        }
    }
}

std::thread_local! {
    /// The thread's [`Buffers`], when none is lent.
    static BUFFERS: core::cell::Cell<Option<Buffers>> = const { core::cell::Cell::new(None) };
}

impl blaze_mem::scratch::Shelved for Buffers {
    fn empty(&mut self) {
        let Buffers { out, working } = self;
        let Outline { items, attrs, file_attrs, errors, messages, keys, own_flags, flags } = out;
        items.clear();
        attrs.clear();
        errors.clear();
        messages.clear();
        keys.clear();
        *file_attrs = AttrSpan::default();
        *own_flags = ItemFlags::NONE;
        *flags = ItemFlags::NONE;
        working.lexer.errors.clear();
        working.lexer.messages.clear();
    }

    fn held(&self) -> usize {
        self.out.items.capacity() + self.working.skeleton.capacity()
    }

    fn unshelve() -> Option<Buffers> {
        BUFFERS.try_with(core::cell::Cell::take).ok().flatten()
    }

    fn shelve(self) {
        // A thread that is ending: the buffers are freed.
        let _ = BUFFERS.try_with(|shelf| shelf.set(Some(self)));
    }
}

/// The working vectors of a walk, which no outline keeps: the skeleton and its cut bodies, and
/// the walk's stacks. One set serves every part of a file in turn, cleared and never freed, so
/// in a task's frame a file's walks hold one set at the size of its largest part, not one per
/// part (rule 11; docs/v2/plan/design/memory.md, the frame reset per item). Beside them, the
/// lexer's errors of every part, which go after all the parser's.
pub(crate) struct Working<'a> {
    skipped: AVec<'a, Skipped>,
    skeleton: AVec<'a, u8>,
    stacks: Stacks<'a>,
    lexer: Lexer<'a>,
    digest: crate::digest::Scratch<'a>,
}

/// The lexer's errors of the parts walked so far, in file offsets, and their messages one
/// after another: empty, and nothing allocated, in nine files in ten.
struct Lexer<'a> {
    errors: AVec<'a, SyntaxError>,
    messages: AVec<'a, u8>,
}

impl<'a> Working<'a> {
    /// Empty: nothing is allocated until the first walk.
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Working<'a> {
        Working {
            skipped: Vec::new_in(alloc),
            skeleton: Vec::new_in(alloc),
            stacks: Stacks::new_in(alloc),
            lexer: Lexer { errors: Vec::new_in(alloc), messages: Vec::new_in(alloc) },
            digest: crate::digest::Scratch::new_in(alloc),
        }
    }
}

/// The stacks a [`Walk`] keeps while it runs and gives back at [`Walk::finish`], empty.
struct Stacks<'a> {
    frames: AVec<'a, Frame>,
    open: AVec<'a, Open>,
    tokens: AVec<'a, Tok>,
}

impl<'a> Stacks<'a> {
    fn new_in(alloc: Alloc<'a>) -> Stacks<'a> {
        Stacks { frames: Vec::new_in(alloc), open: Vec::new_in(alloc), tokens: Vec::new_in(alloc) }
    }
}

/// Walks `part` of `file` into `out`, the file's outline, its digests not yet taken (each new
/// entry's `hash` holds its sibling key's makings, crate::digest::Keyed): file ranges are the
/// part's moved by its start, entries and attributes are counted on from those `out` holds.
/// Its working vectors are `working`'s, given back cleared.
fn walk_part<'a>(
    file: &str,
    part: Range,
    edition: Edition,
    bodies: Bodies,
    working: &mut Working<'a>,
    out: &mut Outline<'a>,
) {
    let text = &file[part.to_usize()];
    working.skipped.clear();
    working.skeleton.clear();
    if bodies == Bodies::Skipped {
        skeleton(text, Cpu::BASELINE, &mut working.skipped, &mut working.skeleton);
    }
    let (skipped, skeleton) = (&working.skipped, &working.skeleton);
    let parsed_text = match bodies {
        Bodies::Parsed => text,
        Bodies::Skipped => match blaze_lex::validate(Cpu::BASELINE, skeleton) {
            Ok(valid) => valid.as_str(),
            Err(invalid) => unreachable!("a text cut at ASCII braces is UTF-8, not at byte {}", invalid.valid_up_to),
        },
    };
    let lexed = LexedStr::new(edition, parsed_text);
    let input = lexed.to_input(edition);
    let output = match bodies {
        Bodies::Parsed => parser::TopEntryPoint::SourceFile.parse(input.tokens()),
        Bodies::Skipped => parser::TopEntryPoint::SourceFile.parse_outline(input.tokens()),
    };
    let stacks = core::mem::replace(&mut working.stacks, Stacks::new_in(*working.skeleton.allocator()));
    let mut walk = Walk::new(text, part.start, &lexed, skipped, stacks, edition, out, &mut working.lexer);
    for step in output.steps() {
        match step {
            OutlineStep::Step(step) => walk.step(step),
            OutlineStep::Lazy { kind, n_input_tokens } => {
                assert_eq!(kind, BLOCK_EXPR, "an outline leaves only a `fn` body unparsed");
                walk.lazy_body(&input, n_input_tokens);
            }
        }
    }
    working.stacks = walk.finish();
}

/// Whether the parser has a `{ .. }` body that holds an item or a macro call at any depth
/// (an `is_item` node) among input tokens `from..from + len`, when the tokens may say so: false
/// only when none of them can start one. Every item but a macro call starts with its keyword
/// (`fn`, `struct`, `enum`, `trait`, `impl`, `const`, `static`, `type`, `mod`, `use`,
/// `extern`, `macro`, or the contextual `union`); `macro_rules!` and every macro call have a
/// `!` right after a path, whose last token is an identifier, a path keyword or a generic
/// argument list's `>`. The grammar takes a `!` there as a call's (`path_expr`, `type_path`,
/// `path_or_macro_pat`) unless it is a joint `!=` in an expression or type; only a pattern
/// right before a joint `!=` (`let a != b`), which nothing valid has, is a call this misses.
/// A `const` right after `*` is a pointer type's, never an item's.
fn may_hold_items(input: &Input, from: usize, len: usize) -> bool {
    let mut prev = EOF;
    for at in from..from + len {
        let (kind, contextual, _, joint, _) = input.token(at);
        let item = match kind {
            FN_KW | STRUCT_KW | ENUM_KW | TRAIT_KW | IMPL_KW | STATIC_KW | TYPE_KW | MOD_KW
            | USE_KW | EXTERN_KW | MACRO_KW => true,
            CONST_KW => prev != STAR,
            IDENT => contextual == UNION_KW,
            BANG => {
                matches!(prev, IDENT | SELF_KW | SUPER_KW | CRATE_KW | SELF_TYPE_KW | R_ANGLE)
                    && !(joint && input.token(at + 1).0 == EQ)
            }
            _ => false,
        };
        if item {
            return true;
        }
        prev = kind;
    }
    false
}

/// How an outline reads a `fn` body: lexed and parsed with the rest, or left unread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bodies {
    /// Every body lexed and parsed: the reference the skipping walk is tested equal to.
    Parsed,
    /// A body whose bytes cannot hold an item or a macro call is not lexed (crate::skeleton);
    /// one the parser may leave unparsed (`parse_outline`, `lazy_block_len`) is parsed only when
    /// its tokens may hold one ([`may_hold_items`]). A body's own syntax errors (the lexer's
    /// and the parser's) are not the outline's then: they are found when the body is parsed
    /// for its own record.
    Skipped,
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
    /// Where its first token outside its attributes and before its body starts: its head's
    /// start, for an entry with no name.
    head_start: Option<u32>,
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
    visibility: WrittenVis<Range>,
    rename: Option<Rename<Range>>,
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
    /// What its own tokens so far set (crate::flags).
    flags: ItemFlags,
}

struct Walk<'t, 'l, 'o, 'a> {
    /// The part of the file walked. The lexer's tokens are the skeleton's (crate::skeleton),
    /// the same as the part's but inside each skipped body. Offsets in the walk are the part's.
    text: &'t str,
    /// Where the part starts in the file: what a file range the walk writes is moved by.
    base: u32,
    lexed: &'l LexedStr<'l>,
    /// The bodies the skeleton cut, in order, and the next one the walk has not passed.
    skipped: &'l [Skipped],
    next_skipped: usize,
    /// The edition a skipped body is lexed in for its flags.
    lex_edition: Edition,
    /// The flags machine, fed every token the parser takes.
    scan: Scan,
    /// The next lexer token.
    pos: usize,
    /// Where token `pos` starts in the file, or the trivia before it.
    offset: u32,
    /// The end of the last token the parser took.
    prev_end: Option<u32>,
    frames: AVec<'a, Frame>,
    open: AVec<'a, Open>,
    /// The depth of the node whose tokens are being recorded into `tokens`.
    recorded: Option<usize>,
    tokens: AVec<'a, Tok>,
    file_children: u32,
    /// The input tokens (trivia not counted) the parser has taken.
    taken: usize,
    /// A body is being walked from its own parse ([`Walk::lazy_body`]): its errors are not the
    /// outline's.
    body_errors_muted: bool,
    /// The file's outline, which the walk appends to.
    out: &'o mut Outline<'a>,
    /// The lexer's errors of the file, which go after all the parser's.
    lexer: &'o mut Lexer<'a>,
}

impl<'t, 'l, 'o, 'a> Walk<'t, 'l, 'o, 'a> {
    #[expect(clippy::too_many_arguments, reason = "the part, its tokens, and where its outline goes")]
    fn new(
        text: &'t str,
        base: u32,
        lexed: &'l LexedStr<'l>,
        skipped: &'l [Skipped],
        stacks: Stacks<'a>,
        edition: Edition,
        out: &'o mut Outline<'a>,
        lexer: &'o mut Lexer<'a>,
    ) -> Self {
        let Stacks { frames, open, tokens } = stacks;
        debug_assert!(frames.is_empty() && open.is_empty(), "a walk's stacks come back empty");
        Walk {
            text,
            base,
            lexed,
            skipped,
            next_skipped: 0,
            lex_edition: edition,
            scan: Scan::new(),
            pos: 0,
            offset: 0,
            prev_end: None,
            frames,
            open,
            recorded: None,
            tokens,
            file_children: 0,
            taken: 0,
            body_errors_muted: false,
            out,
            lexer,
        }
    }

    /// `flags` are the innermost open entry's, or the file's outside every entry.
    fn flag(&mut self, flags: ItemFlags) {
        if flags.is_empty() {
            return;
        }
        match self.open.last_mut() {
            Some(open) => open.flags |= flags,
            None => self.out.own_flags |= flags,
        }
    }

    /// The flags of a skipped body's tokens, `{` to `}` (so no shebang or frontmatter is read
    /// at its start), from a fresh machine: where a whole-file walk stands after the `{`.
    fn body_flags(&self, body: Skipped) -> ItemFlags {
        let at = |offset: u32| usize::try_from(offset).expect("a file offset fits usize");
        let lexed = LexedStr::new(self.lex_edition, &self.text[at(body.open)..=at(body.close)]);
        let mut scan = Scan::new();
        let mut flags = ItemFlags::NONE;
        // The `{` and `}` decide nothing from where a fresh machine stands.
        for token in 0..lexed.len() {
            let kind = lexed.kind(token);
            if !kind.is_trivia() {
                flags |= scan.token(kind, || lexed.text(token));
            }
        }
        flags
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

    fn step(&mut self, step: Step<'_>) {
        match step {
            Step::Token { n_input_tokens, .. } => {
                for _ in 0..n_input_tokens {
                    self.token();
                }
            }
            // `x.0.1`: one float token, then the node it ends and, past a second field, the
            // one the parser left open for it (`Output`'s depth accounting).
            Step::FloatSplit { ends_in_dot } => {
                self.token();
                self.exit();
                if !ends_in_dot {
                    self.exit();
                }
            }
            Step::Enter { kind } => self.enter(kind),
            Step::Exit => self.exit(),
            Step::Error { msg } if !self.body_errors_muted => self.parser_error(msg),
            Step::Error { .. } => {}
        }
    }

    /// A `fn` body the parser left unparsed, its next `len` input tokens: walked as the block it
    /// is, and parsed only when it may hold an item or a macro call.
    fn lazy_body(&mut self, input: &Input, len: u32) {
        let len = usize::try_from(len).expect("a body's tokens are counted in usize");
        let from = self.taken;
        if !may_hold_items(input, from, len) {
            self.enter(BLOCK_EXPR);
            for _ in 0..len {
                self.token();
            }
            self.exit();
            return;
        }
        // The body's tokens alone, as the file's input holds them: a block's parse alone is the
        // subtree a full parse makes there (crates/parser/src/outline.rs).
        let mut body = Input::with_capacity(len);
        for at in from..from + len {
            let (kind, contextual, edition, joint, _) = input.token(at);
            match kind {
                IDENT => body.push_ident(contextual, edition),
                _ => body.push(kind, edition),
            }
            if joint {
                body.was_joint();
            }
        }
        let parsed = Reparser::for_node(BLOCK_EXPR, None, None)
            .expect("a block expression has a reparser")
            .parse(body.tokens());
        self.body_errors_muted = true;
        for step in parsed.steps() {
            match step {
                OutlineStep::Step(step) => self.step(step),
                OutlineStep::Lazy { kind, .. } => unreachable!("a full parse's output holds a lazy {kind:?}"),
            }
        }
        self.body_errors_muted = false;
        assert_eq!(self.taken, from + len, "a body's parse takes every token of the body");
    }

    /// The parser took one raw token.
    fn token(&mut self) {
        self.taken += 1;
        self.skip_trivia();
        assert!(self.pos < self.lexed.len(), "the parser took a token past the lexer's last");
        let kind = self.lexed.kind(self.pos);
        let range = Range::new(self.offset, self.offset + self.token_len(self.pos));
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
        if let Some(open) = self.open.last_mut()
            && !open.body_open
            && !open.attr_open
        {
            open.head_start.get_or_insert(range.start);
        }
        for open in self.open.iter_mut() {
            open.start.get_or_insert(range.start);
            open.last_end = Some(range.end);
        }
        self.prev_end = Some(range.end);
        self.offset = range.end;
        self.pos += 1;
        let text = self.text;
        let flags = self.scan.token(kind, || range.of(text));
        self.flag(flags);
        // A skipped body's `{`: its inside is not in the skeleton, and its `}` is next.
        if let Some(&body) = self.skipped.get(self.next_skipped)
            && range.start == body.open
        {
            assert_eq!(kind, L_CURLY, "a skipped body starts at its `{{`");
            self.offset = body.close;
            self.next_skipped += 1;
            if body.may_flag {
                let flags = self.body_flags(body);
                self.flag(flags);
            }
        }
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
        self.open.push(Open {
            index,
            parent,
            syntax,
            head_start: None,
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
            visibility: WrittenVis::Private,
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
            flags: ItemFlags::NONE,
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
        let attrs = &self.out.attrs[open.attrs.to_usize()];
        let kind = match open.syntax {
            FN => ItemDetail::Fn { body_holds_items: open.holds_items },
            STRUCT => ItemDetail::Struct,
            ENUM => ItemDetail::Enum,
            UNION => ItemDetail::Union,
            TRAIT if open.before_eq.is_some() => ItemDetail::TraitAlias,
            TRAIT => ItemDetail::Trait,
            IMPL => ItemDetail::Impl,
            CONST => ItemDetail::Const { body_holds_items: open.holds_items },
            STATIC => ItemDetail::Static { body_holds_items: open.holds_items },
            TYPE_ALIAS => ItemDetail::TypeAlias,
            MODULE => ItemDetail::Mod {
                inline: open.body.is_some(),
                path: attrs.iter().find_map(|attr| match attr.kind {
                    AttrKind::Path { literal } if !attr.inner => Some(literal),
                    _ => None,
                }),
            },
            USE => ItemDetail::Use { tree: open.tree.map(rel) },
            EXTERN_CRATE => ItemDetail::ExternCrate {
                rename: open.rename.map_or(Rename::Same, |rename| rename.map(rel)),
            },
            EXTERN_BLOCK => ItemDetail::ExternBlock { abi: open.abi.map(rel) },
            MACRO_RULES => ItemDetail::MacroRules {
                exported: attrs.iter().any(|attr| !attr.inner && attr.kind == AttrKind::MacroExport),
            },
            MACRO_DEF => ItemDetail::MacroDef,
            MACRO_CALL => {
                ItemDetail::MacroCall { path: open.path.map(rel), input: open.input.map(rel) }
            }
            VARIANT => ItemDetail::Variant { discriminant: open.value.map(rel) },
            RECORD_FIELD | TUPLE_FIELD => {
                ItemDetail::Field { ty: open.ty.map(rel), default: open.value.map(rel) }
            }
            ERROR | ATTR | DOC_COMMENT => ItemDetail::Error,
            syntax => ItemDetail::Unhandled { syntax: u16::from(syntax) },
        };
        // The sibling key's makings, in file offsets, kept in the entry's `hash` until it and
        // the digests are hashed when the whole file is walked (crate::digest).
        let base = self.base;
        let by = match open.syntax {
            // A tuple field is told apart by its position alone.
            TUPLE_FIELD => By::Position,
            syntax if is_named(syntax) => By::Name(open.name.map(|name| {
                let raw = name.of(self.text).starts_with("r#");
                Range::new(name.start + if raw { 2 } else { 0 }, name.end).shifted(base)
            })),
            _ => By::Head(
                Range::new(open.head_start.unwrap_or(signature_end).min(signature_end), signature_end).shifted(base),
            ),
        };
        let after = u32::try_from(self.out.items.len()).expect("an outline's entries are counted in u32");
        self.out.items[open.index.at()] = Item {
            detail: kind,
            parent: open.parent,
            end: ItemIndex(after),
            range: Range::new(start, end).shifted(base),
            name: open.name.map(rel),
            visibility: open.visibility.map(rel),
            signature: Range::new(0, signature_end - start),
            body: open.body.map(rel),
            attrs: open.attrs,
            disambiguator: 0,
            hash: Keyed { tag: tag_of(kind), by }.stash(),
            signature_hash: ContentId([0; 32]),
            flags: open.flags,
        };
    }

    fn parser_error(&mut self, message: &str) {
        let range = Range::new(self.offset, self.offset).shifted(self.base);
        push_error(&mut self.out.errors, &mut self.out.messages, range, ErrorOrigin::Parser, message);
    }

    /// The walk done: the part's lexer errors put by for the file's end, and its stacks given
    /// back for the next part's walk, empty.
    fn finish(self) -> Stacks<'a> {
        assert!(
            self.frames.is_empty() && self.open.is_empty(),
            "the parser's output closes every node it opens"
        );
        let lexed = self.lexed;
        for (token, message) in lexed.errors() {
            let at = lexed.text_range(token);
            let range = Range::new(
                self.in_file(u32::try_from(at.start).expect("a file under 4 GiB")),
                self.in_file(u32::try_from(at.end).expect("a file under 4 GiB")),
            )
            .shifted(self.base);
            push_error(&mut self.lexer.errors, &mut self.lexer.messages, range, ErrorOrigin::Lexer, message);
        }
        let mut tokens = self.tokens;
        tokens.clear();
        Stacks { frames: self.frames, open: self.open, tokens }
    }

    /// The part's offset of the skeleton's `offset` (none falls inside a skipped body, whose
    /// skeleton is `{}`).
    fn in_file(&self, offset: u32) -> u32 {
        let mut cut = 0;
        for body in self.skipped {
            // The skeleton's offset of the body's `}`.
            if offset <= body.open - cut {
                break;
            }
            cut += body.cut();
        }
        offset + cut
    }
}

/// Pushes an error at `range` with `message`, copied onto the end of `messages`.
fn push_error(errors: &mut AVec<'_, SyntaxError>, messages: &mut AVec<'_, u8>, range: Range, origin: ErrorOrigin, message: &str) {
    let start = u32::try_from(messages.len()).expect("messages under 4 GiB");
    messages.extend_from_slice(message.as_bytes());
    let end = u32::try_from(messages.len()).expect("messages under 4 GiB");
    errors.push(SyntaxError { range, origin, message: Range::new(start, end) });
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
fn tag_of(kind: ItemDetail) -> u8 {
    match kind {
        ItemDetail::Fn { .. } => 0,
        ItemDetail::Struct => 1,
        ItemDetail::Enum => 2,
        ItemDetail::Union => 3,
        ItemDetail::Trait => 4,
        ItemDetail::TraitAlias => 5,
        ItemDetail::Impl => 6,
        ItemDetail::Const { .. } => 7,
        ItemDetail::Static { .. } => 8,
        ItemDetail::TypeAlias => 9,
        ItemDetail::Mod { .. } => 10,
        ItemDetail::Use { .. } => 11,
        ItemDetail::ExternCrate { .. } => 12,
        ItemDetail::ExternBlock { .. } => 13,
        ItemDetail::MacroRules { .. } => 14,
        ItemDetail::MacroDef => 15,
        ItemDetail::MacroCall { .. } => 16,
        ItemDetail::Variant { .. } => 17,
        ItemDetail::Field { .. } => 18,
        ItemDetail::Error => 19,
        ItemDetail::Unhandled { .. } => 20,
    }
}

/// A visibility from its tokens: `pub`, then `( .. )` or nothing. Ranges are the file's.
fn visibility_of(tokens: &[Tok]) -> WrittenVis<Range> {
    let Some(paren) = tokens.get(1).filter(|token| token.kind == L_PAREN) else {
        return WrittenVis::Pub;
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
        [only] if only.kind == CRATE_KW => WrittenVis::Crate,
        [only] if only.kind == SUPER_KW => WrittenVis::Super,
        [only] if only.kind == SELF_KW => WrittenVis::SelfOnly,
        [first, path @ ..] if first.kind == IN_KW => WrittenVis::In(span(path)),
        path => WrittenVis::WithoutIn(span(path)),
    }
}

/// An `extern crate`'s rename from its tokens: `as`, then a name or `_`.
fn rename_of(tokens: &[Tok]) -> Option<Rename<Range>> {
    match tokens.get(1) {
        Some(token) if token.kind == UNDERSCORE => Some(Rename::Underscore),
        Some(token) => Some(Rename::To(token.range)),
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
