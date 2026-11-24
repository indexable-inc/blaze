//! The machine: the parser's events in, columns out, with no tree between.
//!
//! Two stacks: the frames of the nodes open (their kind, where they start, where their
//! children's values begin, the marks of what they opened), and the values the closed
//! children left (a token, an expression's id, an unplaced row). A node's handler runs when
//! it closes: it reads its children's values, writes its own row into its column and leaves
//! one value (or none) for its parent. Expressions, patterns, types and paths are written at
//! once; list elements (arms, fields, segments, arguments) ride on the stack as rows and are
//! written together when their list closes, so a list is a contiguous run of its column.
//!
//! Scopes are resolved on the way: a pattern's bindings wait in `pending` until their
//! binder says they are in scope (a `let` at its end, an arm after its pattern, a `for`
//! at its body, a closure or fn after its parameters, a `let` expression at its end), then
//! sit in `locals` with a map from name to the innermost one, until their scope closes.
//! A barrier hides the body's locals from an anonymous constant. Loops, labelled blocks,
//! `try` blocks and closures push entries; a `break` or `continue` names its entry, and its
//! target is patched when the entry's node closes, its id known only then.
//!
//! What a node of the source is skipped wholesale (a token tree, a const argument, a local
//! item, a fn's body in a signature) is a depth count: its events are counted, not read.
//! A false `cfg` on a node skips the rest of the node the same way, and the node leaves
//! nothing.

use blaze_hash::HashMap;
use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_surface::{Edition, Range};
use blaze_typeck::Name;
use parser::SyntaxKind::{self, *};

use crate::columns::{Columns, Lens, head};
use crate::form::*;
use crate::columns::View;
use crate::{Cfg, CfgAnswer, Lowered, Need, ValueName, ValueNames, lit};

/// Where a body-local `macro_rules!` template's locals and labels resolve: at its definition.
mod def_site;
/// `asm!`'s `target_arch`, read as `cfg` atoms at the `asm!` (INTRINSICS.md P9b).
mod asm_arch;

/// What an item declared in one of the body's blocks makes `name` at `scope`, for a pattern:
/// `None` when no such item is in the value namespace there (the module's answer is asked).
fn local_value(v: &View<'_>, scope: ScopeId, name: NameIdx) -> Option<ValueName> {
    let hash = v.name(name).expect("a name the lowering wrote");
    let row = crate::resolve::local_item(v, Some(scope), hash).expect("the lowering's own scopes")?;
    let item = v.local_item(v.row(row).expect("a local item's row")).expect("a local item");
    let kind = item.syntax;
    if kind == CONST as u16 {
        Some(ValueName::Const)
    } else if kind == STATIC as u16 {
        Some(ValueName::Static)
    } else if kind == FN as u16 {
        Some(ValueName::Nothing)
    } else if kind == STRUCT as u16 {
        struct_shape(v.text_at(item.range).expect("a local item's text"))
    } else {
        None
    }
}

/// A struct item's value: its constructor when it is a unit or a tuple struct (`struct S;`,
/// `struct S<T>(T);`), `None` for one with named fields, which is no value. Read after the
/// `struct` keyword: the name, the generics, then `;` or `(`.
fn struct_shape(text: &str) -> Option<ValueName> {
    let at = text.match_indices("struct").find(|(i, _)| {
        let before = text[..*i].chars().next_back();
        let after = text[i + 6..].chars().next();
        before.is_none_or(|c| !(c.is_alphanumeric() || c == '_')) && after.is_some_and(char::is_whitespace)
    })?;
    let rest = text[at.0 + 6..].trim_start();
    let rest = rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').trim_start();
    let mut depth = 0u32;
    for c in rest.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            c if depth > 0 || c.is_whitespace() => {}
            ';' => return Some(ValueName::UnitStruct),
            '(' => return Some(ValueName::TupleCtor),
            _ => return None,
        }
    }
    None
}

/// What a lowering makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// An item's signature: its body (a fn's block, a const's value) is skipped.
    Signature,
    /// A fn's body or a const's or static's value: the signature's parts outside its
    /// parameters are skipped.
    Body,
    /// An anonymous constant's expression.
    AnonConst,
    /// One type alone, from its text ([`crate::lower_type_text`]): lowered as a signature's
    /// types are (paths with no body scope), its root at [`head::TY`].
    Type,
}

/// A signature's flags, in the head's `FLAGS` word.
pub mod sig {
    pub const CONST: u32 = 1;
    pub const ASYNC: u32 = 1 << 1;
    pub const UNSAFE: u32 = 1 << 2;
    pub const EXTERN: u32 = 1 << 3;
    pub const DEFAULT: u32 = 1 << 4;
    pub const SAFE: u32 = 1 << 5;
    pub const GEN: u32 = 1 << 6;
    pub const AUTO: u32 = 1 << 7;
    /// `impl !Trait for T`
    pub const NEGATIVE: u32 = 1 << 8;
    pub const HAS_BODY: u32 = 1 << 9;
    /// `static mut`
    pub const MUT: u32 = 1 << 10;
    /// `const _`
    pub const UNDERSCORE: u32 = 1 << 11;
    /// `trait A = B;`
    pub const TRAIT_ALIAS: u32 = 1 << 12;
    /// A parameter list ending in `...`.
    pub const VARIADIC: u32 = 1 << 13;
}

// Frame flags.
const F_DEAD: u32 = 1;
const F_LOCALS: u32 = 1 << 1;
const F_BARRIER: u32 = 1 << 2;
const F_LOOP: u32 = 1 << 3;
const F_CORO: u32 = 1 << 4;
const F_SCOPE: u32 = 1 << 5;
const F_EQ: u32 = 1 << 6;
pub(crate) const F_ASYNC: u32 = 1 << 7;
pub(crate) const F_GEN: u32 = 1 << 8;
const F_CONST: u32 = 1 << 9;
const F_UNSAFE: u32 = 1 << 10;
pub(crate) const F_MOVE: u32 = 1 << 11;
const F_TRY: u32 = 1 << 12;
const F_STATIC: u32 = 1 << 13;
const F_SEMI: u32 = 1 << 14;
const F_ACTIVE: u32 = 1 << 15;
const F_SKIP: u32 = 1 << 16;
const F_FOR: u32 = 1 << 17;
const F_POPPED: u32 = 1 << 18;
/// An item of a block whose outer attributes are being read (`Machine::enter`): a false `cfg`
/// among them removes the item, as it removes a statement; at its first other child or token
/// the rest of it is skipped wholesale (`Machine::attrs_read`).
const F_ATTRS: u32 = 1 << 19;
/// Comptime field-unrolling `inline for pat in iter { body }`: preserved as `Expr::InlineFor`
/// instead of desugared to `IntoIterator`.
const F_INLINE: u32 = 1 << 20;

const NO_RANGE: Range = Range::new(NONE, NONE);

/// On the coroutine stack: a plain closure, which hides the coroutine around it.
pub(crate) const NOT_CORO: u32 = NONE - 1;

/// An open node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub(crate) kind: SyntaxKind,
    /// Its first token's start, once it has one.
    pub(crate) start: u32,
    /// Where its children's values start on the value stack.
    pub(crate) base: u32,
    /// The locals and pending bindings when it opened.
    pub(crate) locals: u32,
    pub(crate) pending: u32,
    pub(crate) saved_barrier: u32,
    /// A `for` loop's label, a name.
    pub(crate) aux: u32,
    pub(crate) flags: u32,
}

/// What a closed child left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum V {
    Tok(SyntaxKind),
    Expr,
    Pat,
    Type,
    Stmt,
    Name,
    NameRef(SyntaxKind),
    Lifetime,
    /// A literal in a pattern: the node holds its encoding.
    Lit,
    /// A path segment, unplaced; `node.f` is 1 after a leading `::`.
    Seg,
    Path,
    /// An unplaced row of `tag`.
    Row(u16),
    /// A placed list: `id` its start, `node.a` its length, `node.b`/`node.c` its extra.
    List(L),
    /// A node skipped wholesale; `node.a..node.b` the range of its name, when it has one.
    Skipped(SyntaxKind),
    CfgFalse,
    Pred(CfgAnswer),
    Ret,
    /// `node.a` 1 with an ABI string, at the value's range.
    Abi,
    /// `node.flags` the visibility's code, `node.a` its path.
    Vis,
    Else,
    Guard,
    /// `node.a`/`node.b` the statements, `node.c` the tail, `node.d` the scope.
    StmtList,
    /// `for<F in fields(T)>`: `id` the bound parameter's `NameIdx`, `node.a` the container's `TypeId`.
    ForFields,
}

/// What a placed list holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum L {
    Exprs,
    Arms,
    Params,
    GArgs,
    Bounds,
    GParams,
    Where,
    Fields(Shape),
    Variants,
    Inits,
    PatFields,
    Items,
    Names,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Val {
    pub(crate) v: V,
    pub(crate) id: u32,
    pub(crate) node: Node,
    pub(crate) range: Range,
}

impl Val {
    fn of(v: V, id: u32, range: Range) -> Val {
        Val { v, id, node: Node::of(0), range }
    }
}

/// An identifier pattern as written: its `Pat::Bind`, the block it is in, and whether it is a
/// bare name (no `ref`, `mut` or `@`), the one form a path pattern can take.
#[derive(Clone, Copy)]
struct Ident {
    pat: u32,
    scope: u32,
    bare: bool,
}

/// A binding in scope.
#[derive(Clone, Copy)]
struct Local {
    name: u64,
    pat: u32,
    /// The binding of the same name it hides, an index of `locals`.
    prev: u32,
}

/// What a `break` or `continue` can leave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Jk {
    Loop,
    Block,
    Try,
    /// A closure, a coroutine or an anonymous constant: nothing outside is reachable.
    Barrier,
}

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) kind: Jk,
    /// The label's name hash, 0 with none.
    pub(crate) label: u64,
    /// Where its jumps start in `jumps`.
    pub(crate) jumps: u32,
}

/// The machine. `'t` the item's text, `'c` the cfg set, `'a` the allocator of its output.
pub struct Machine<'t, 'c, 'a> {
    text: &'t str,
    pub(crate) mode: Mode,
    pub(crate) edition: Edition,
    /// The cfg set; `None` only while it is lent to the expander for one call
    /// ([`Machine::lend_cfg`]), when no step runs.
    cfg: Option<&'c mut dyn Cfg>,
    /// What a name is at the body's module, for identifier patterns; `None` for a signature
    /// or a type, whose patterns (a signature's parameters) no check reads.
    values: Option<&'c mut dyn ValueNames>,
    /// Every identifier pattern, settled by [`Machine::settle_patterns`] at the end.
    idents: AVec<'a, Ident>,
    pub(crate) out: Columns<'a>,
    frames: AVec<'a, Frame>,
    pub(crate) vals: AVec<'a, Val>,
    last_end: u32,
    /// Inside a node skipped wholesale: the depth below it, 0 outside.
    skip: u32,
    skip_name: Range,
    skip_arm: bool,
    /// The frame of the item lowered; `usize::MAX` for an anonymous constant.
    root_depth: usize,
    locals: AVec<'a, Local>,
    top: HashMap<u64, u32, Alloc<'a>>,
    pending: AVec<'a, (u64, u32)>,
    /// Locals below it are hidden.
    barrier: u32,
    pub(crate) entries: AVec<'a, Entry>,
    /// (entry, break or continue expression) waiting for the entry's node to close.
    jumps: AVec<'a, (u32, u32)>,
    /// The bodies open that `.await` and `yield` see: each a coroutine's kind (its `F_ASYNC`
    /// and `F_GEN` flags), or [`NOT_CORO`] for a plain closure, which hides the coroutine
    /// around it.
    pub(crate) coros: AVec<'a, u32>,
    scopes: AVec<'a, u32>,
    needs: AVec<'a, Range>,
    /// The expression a closing frame's entry targets, set by its handler.
    pub(crate) loop_target: u32,
    alloc: Alloc<'a>,
    /// The item's text and, past its end, each expansion lowered in place, one after another:
    /// made at the first expansion; until then the item's text is read where it is.
    grown: Option<AString<'a>>,
    /// The words of the expansions in `grown`, in order: where each starts, its local
    /// hygiene ([`blaze_expand::local_hygiene`]), and which expansion and token it is.
    words: AVec<'a, ExpWord>,
    /// A body's macro call the feed is to expand, set when the call's node closes.
    request: Option<Request>,
    /// The `macro_rules!` the body has defined so far, in textual order, each with the depth
    /// of `scopes` it was written at: the Reference's textual scope ("Macros By Example",
    /// Scoping): visible after its statement, in nested blocks too, until its block ends.
    local_macros: AVec<'a, LocalMacroAt>,
    /// Each local hygiene an expansion's words carry, with the definition its template
    /// resolves locals and labels at (def_site.rs).
    template_sites: AVec<'a, def_site::TemplateSite>,
    /// Whether the feed has an expander: without one a call stays a macro call node.
    pub(crate) expanding: bool,
}

/// One word of an expansion lowered in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExpWord {
    /// The word's byte range in the machine's text, excluding inter-word spacing.
    pub(crate) text: Range,
    pub(crate) hygiene: u64,
    pub(crate) expansion: u32,
    pub(crate) token: u32,
    /// One past the word's last token: `token + 2` for a lifetime (`'` and its name).
    pub(crate) end: u32,
}

/// A `macro_rules!` a body defines: its name's range and the item's, and the depth of the
/// machine's `scopes` (the blocks open) where it is written.
#[derive(Clone, Copy, Debug)]
struct LocalMacroAt {
    name: Range,
    item: Range,
    depth: u32,
    /// How many `locals` were bound and `entries` open where it is written: the locals and
    /// labels its template sees (def_site.rs).
    locals: u32,
    entries: u32,
    /// A `macro` (2.0) item, not a `macro_rules!`.
    macro2: bool,
}

/// A body's macro call waiting for its expansion: the call's range, its path's and its
/// input's (delimiters included), and the fragment its position takes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub(crate) range: Range,
    pub(crate) path: PathIdx,
    pub(crate) path_range: Range,
    pub(crate) input: Range,
    pub(crate) place: MacroPlace,
    pub(crate) fragment: blaze_expand::Fragment,
}

pub(crate) fn idx(len: usize) -> u32 {
    u32::try_from(len).expect("a column of over u32::MAX rows: an item of under 4 GiB never makes one")
}

fn is_item(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        CONST
            | ENUM
            | EXTERN_BLOCK
            | EXTERN_CRATE
            | FN
            | IMPL
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

fn is_type(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        ARRAY_TYPE
            | DYN_TRAIT_TYPE
            | FN_PTR_TYPE
            | FOR_TYPE
            | IMPL_TRAIT_TYPE
            | INFER_TYPE
            | MACRO_TYPE
            | NEVER_TYPE
            | PAREN_TYPE
            | PATH_TYPE
            | PTR_TYPE
            | REF_TYPE
            | SLICE_TYPE
            | TUPLE_TYPE
            | PATTERN_TYPE
            | UNSAFE_BINDER_TYPE
    )
}

fn is_binder(kind: SyntaxKind) -> bool {
    matches!(kind, LET_STMT | MATCH_ARM | PARAM_LIST | FOR_EXPR | LET_EXPR)
}

fn bin_op(kind: SyntaxKind) -> Option<(BinOp, bool)> {
    Some(match kind {
        PLUS => (BinOp::Add, false),
        MINUS => (BinOp::Sub, false),
        STAR => (BinOp::Mul, false),
        SLASH => (BinOp::Div, false),
        PERCENT => (BinOp::Rem, false),
        AMP => (BinOp::BitAnd, false),
        PIPE => (BinOp::BitOr, false),
        CARET => (BinOp::BitXor, false),
        SHL => (BinOp::Shl, false),
        SHR => (BinOp::Shr, false),
        EQ2 => (BinOp::Eq, false),
        NEQ => (BinOp::Ne, false),
        L_ANGLE => (BinOp::Lt, false),
        LTEQ => (BinOp::Le, false),
        R_ANGLE => (BinOp::Gt, false),
        GTEQ => (BinOp::Ge, false),
        AMP2 => (BinOp::And, false),
        PIPE2 => (BinOp::Or, false),
        PLUSEQ => (BinOp::Add, true),
        MINUSEQ => (BinOp::Sub, true),
        STAREQ => (BinOp::Mul, true),
        SLASHEQ => (BinOp::Div, true),
        PERCENTEQ => (BinOp::Rem, true),
        AMPEQ => (BinOp::BitAnd, true),
        PIPEEQ => (BinOp::BitOr, true),
        CARETEQ => (BinOp::BitXor, true),
        SHLEQ => (BinOp::Shl, true),
        SHREQ => (BinOp::Shr, true),
        _ => return None,
    })
}

impl<'t, 'c, 'a> Machine<'t, 'c, 'a> {
    pub(crate) fn new(
        text: &'t str,
        mode: Mode,
        edition: Edition,
        cfg: &'c mut dyn Cfg,
        values: Option<&'c mut dyn ValueNames>,
        alloc: Alloc<'a>,
    ) -> Machine<'t, 'c, 'a> {
        let mut out = Columns::new_in(alloc);
        out.head[head::MODE] = match mode {
            Mode::Signature => 0,
            Mode::Body => 1,
            Mode::AnonConst => 2,
            Mode::Type => 3,
        };
        out.head[head::EDITION] = crate::edition_byte(edition) as u32;
        out.head[head::FLAGS] = 0;
        out.head[head::CFG_OUT] = 0;
        out.head[head::ITEM_LEN] = idx(text.len());
        for word in [head::GENERICS, head::WHERE, head::ATTRS, head::PARAMS, head::FIELDS, head::BOUNDS, head::ITEMS] {
            out.head[word] = 0;
            out.head[word + 1] = 0;
        }
        let mut m = Machine {
            text,
            mode,
            edition,
            cfg: Some(cfg),
            values,
            idents: Vec::new_in(alloc),
            out,
            frames: Vec::new_in(alloc),
            vals: Vec::new_in(alloc),
            last_end: 0,
            skip: 0,
            skip_name: NO_RANGE,
            skip_arm: false,
            root_depth: usize::MAX,
            locals: Vec::new_in(alloc),
            top: HashMap::new_in(alloc),
            pending: Vec::new_in(alloc),
            barrier: 0,
            entries: Vec::new_in(alloc),
            jumps: Vec::new_in(alloc),
            coros: Vec::new_in(alloc),
            scopes: Vec::new_in(alloc),
            needs: Vec::new_in(alloc),
            loop_target: NONE,
            alloc,
            grown: None,
            words: Vec::new_in(alloc),
            request: None,
            local_macros: Vec::new_in(alloc),
            template_sites: Vec::new_in(alloc),
            expanding: false,
        };
        if matches!(mode, Mode::Body | Mode::AnonConst) {
            // The body's outermost scope: its parameters' and its module's.
            m.out.scopes.push(Scope { parent: None, items: List { start: 0, len: 0 } }.encode());
            m.scopes.push(0);
        }
        m
    }

    /// The text every range is of: the item's, then the expansions lowered in place.
    pub(crate) fn text(&self) -> &str {
        match &self.grown {
            Some(grown) => grown.as_str(),
            None => self.text,
        }
    }

    pub(crate) fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Which text wrote the block whose `{` is at `open` of the machine's text
    /// (crate::scope_origin): the item's own, or the expansion whose word the `{` is. Every byte
    /// past the item's text is a word of an expansion or the space before one
    /// (`append_expansion`), and a `{` is a word, which carries its expansion's index; the
    /// words are appended in text order.
    fn origin_of_block(&self, open: u32) -> crate::scope_origin::ScopeOrigin {
        use crate::scope_origin::ScopeOrigin;
        let item = idx(self.text.len());
        if open < item {
            return ScopeOrigin::Item { open };
        }
        let at = self.words.binary_search_by_key(&open, |w| w.text.start).expect("a block's `{` past the item's text is a word of an expansion");
        let expansion = self.words[at].expansion;
        let start = self.out.expansion_starts[expansion as usize];
        ScopeOrigin::Expansion { expansion, open: open - item - start }
    }

    /// The local hygiene of the word containing this byte. An implicit format capture
    /// names a substring of a string token and inherits the hygiene of that literal.
    fn hygiene_at(&self, byte: u32) -> u64 {
        if (byte as usize) < self.text.len() {
            return 0;
        }
        let i = self.words.partition_point(|w| w.text.end <= byte);
        self.words.get(i).filter(|w| w.text.start <= byte).map_or(0, |w| w.hygiene)
    }

    /// The key a local or a label of name `name` is bound and looked up by: its name's hash,
    /// and its local hygiene when an expansion's own tokens wrote it (blaze-expand's `body`
    /// module states the rule).
    pub(crate) fn local_key(&self, name: NameIdx) -> u64 {
        let hash = self.out.names[name.0 as usize];
        def_site::keyed(hash, self.hygiene_at(self.out.name_ranges[name.0 as usize].start))
    }

    /// The cfg set, lent to the expander for one call: a `cfg!` in the body is answered by the
    /// set its `#[cfg]`s are, never by a host's own answer. Given back by [`Machine::return_cfg`]
    /// before the next step.
    pub(crate) fn lend_cfg(&mut self) -> &'c mut dyn Cfg {
        let Some(cfg) = self.cfg.take() else { unreachable!("the cfg set is lent once at a time") };
        cfg
    }

    pub(crate) fn return_cfg(&mut self, cfg: &'c mut dyn Cfg) {
        self.cfg = Some(cfg);
    }

    /// The call the feed is to expand, once.
    pub(crate) fn take_request(&mut self) -> Option<Request> {
        self.request.take()
    }

    /// The `macro_rules!` and `macro` the body wrote that are in scope here, in the order
    /// written: each one's name and its definition's text ([`def_site::form_of`]).
    pub(crate) fn local_macros(&self) -> impl Iterator<Item = crate::driver::LocalMacro<'_>> + '_ {
        let text = self.text();
        self.local_macros
            .iter()
            .filter_map(move |l| def_site::form_of(text, l).map(|form| crate::driver::LocalMacro { name: l.name.of(text), form }))
    }

    /// The marks of `request`'s input's words, as [`Machine::mark_at`] reads them: the rows
    /// whose start is inside the input (the item's own words', or an expansion's when the input
    /// text spans one; [`Machine::call_input`]).
    pub(crate) fn input_marks(&self, request: &Request) -> crate::driver::InputMarks<'_> {
        use blaze_expand::origin_column::MARK;
        let r = request.input;
        let starts = &self.out.mark_starts;
        let first = starts.partition_point(|&s| s < r.start);
        let past = starts.partition_point(|&s| s < r.end);
        crate::driver::InputMarks::new(r.start, &starts[first..past], &self.out.mark_bytes[first * MARK..past * MARK])
    }

    /// Where `request`'s input is: the item's text, or tokens of an expansion lowered here.
    pub(crate) fn call_input(&self, request: &Request) -> crate::driver::CallInput<'_> {
        let r = request.input;
        if (r.end as usize) <= self.text.len() {
            return crate::driver::CallInput::Text(r.of(self.text));
        }
        let first = self.words.partition_point(|w| w.text.start < r.start);
        let last = self.words.partition_point(|w| w.text.start < r.end).saturating_sub(1);
        match (self.words.get(first), self.words.get(last)) {
            (Some(a), Some(b)) if a.expansion == b.expansion => crate::driver::CallInput::Expanded {
                expansion: a.expansion,
                start: a.token,
                end: b.end,
            },
            _ => crate::driver::CallInput::Text(r.of(self.text())),
        }
    }

    /// Puts the marks of the item's own words beside its text (drive.md, "A token keeps its
    /// mark"): the item a macro wrote keeps each word's mark at the word's start, before any
    /// expansion's (whose starts are past the item's text), so the starts ascend. Called once,
    /// before the feed. `marks` is of this machine's text: [`ItemMarks::of`] proved it a slice
    /// of the expansion's text, and the lowering takes its text from it.
    ///
    /// [`ItemMarks::of`]: crate::mark_site::ItemMarks::of
    pub(crate) fn mark_item(&mut self, marks: crate::mark_site::ItemMarks<'_>) {
        debug_assert!(core::ptr::eq(marks.text(), self.text), "the item's marks are of its own text");
        debug_assert!(self.out.mark_starts.is_empty(), "the item's marks come before any expansion's");
        for (start, mark) in marks.marked() {
            self.out.mark_starts.push(start);
            self.out.mark_bytes.extend_from_slice(&mark.to_bytes());
        }
    }

    /// The call written in the item's own text that `range` is in: `range` itself when it is
    /// there, else the call whose expansion holds it, followed out to the text (an expansion is
    /// appended after every one before it, so each step lands on an earlier call, and the walk
    /// ends). A path the call's expansion reads at the call site (no `$crate`, no opaque mark:
    /// the Reference, "Macros By Example", Hygiene) is read where that written call is: in the
    /// innermost block of the item's text around it.
    pub(crate) fn written_call(&self, range: Range) -> Range {
        let base = idx(self.text.len());
        let mut at = range;
        while at.start >= base {
            let offset = at.start - base;
            let after = self.out.expansion_starts.partition_point(|s| *s <= offset);
            let Some(e) = after.checked_sub(1) else { break };
            let call = self.out.expansion_calls[e];
            // An expansion's call is written before its expansion is appended: each step goes
            // to an earlier start.
            assert!(call.start < at.start, "a call comes before its expansion");
            at = call;
        }
        at
    }

    /// The expansions appended so far, each one's text and call (the driver's
    /// [`crate::driver::Expansions`]): a resolver reads a call's block in them.
    pub(crate) fn expansions(&self) -> crate::driver::Expansions<'_> {
        crate::driver::Expansions { text: self.text(), item_len: idx(self.text.len()), starts: &self.out.expansion_starts, calls: &self.out.expansion_calls }
    }

    /// The mark of the word that starts at `start` (the item's own text or an expansion lowered
    /// in place): `None` for a word no macro wrote.
    pub(crate) fn mark_at(&self, start: u32) -> Option<crate::mark_site::TextMark> {
        use blaze_expand::origin_column::MARK;
        let i = self.out.mark_starts.binary_search(&start).ok()?;
        let bytes = &self.out.mark_bytes[i * MARK..(i + 1) * MARK];
        // The machine wrote these bytes with `TextMark::to_bytes`, and only it.
        Some(crate::mark_site::TextMark::from_bytes(bytes).expect("a mark the machine wrote reads back"))
    }

    /// Appends an expansion's words to the text: each word's text, its first token and its
    /// local hygiene; records the call's read (its range and the expansion's key). Gives each
    /// word's range, for the feed.
    pub(crate) fn append_expansion(&mut self, request: &Request, out: &crate::driver::BodyOut<'_>) -> AVec<'a, Range> {
        let grown = self.grown.get_or_insert_with(|| {
            let mut s = AString::new_in(self.alloc);
            s.push_str(self.text);
            s
        });
        let expansion = idx(self.out.expansion_calls.len());
        self.out.expansion_calls.push(request.range);
        self.out.expansion_keys.push(out.key);
        self.out.expansion_starts.push(idx(grown.len() - self.text.len()));
        let mut ranges = Vec::with_capacity_in(out.words.words.len(), self.alloc);
        for w in out.words.words.iter() {
            // A space before each word that may take one: two words lex again as they were
            // (`a b` stays two, `::` stays one), as `format_args!` reads its input's text.
            if w.space_before {
                grown.push(' ');
            }
            let start = idx(grown.len());
            grown.push_str(&out.words.text[w.text.0 as usize..w.text.1 as usize]);
            let end = idx(grown.len());
            let text = Range::new(start, end);
            ranges.push(text);
            // The word's mark beside the text (drive.md, "A token keeps its mark"): starts
            // ascend, as the words are appended in order.
            if let Some(mark) = w.mark {
                self.out.mark_starts.push(start);
                self.out.mark_bytes.extend_from_slice(&mark.to_bytes());
            }
            self.words.push(ExpWord { text, hygiene: w.hygiene, expansion, token: w.token, end: w.end });
        }
        self.note_templates(request, &out.words.words);
        ranges
    }

    /// A call not expanded: kept as a macro call node, counted as unhandled with `why`.
    pub(crate) fn unexpanded(&mut self, request: Request, why: Why) {
        let r = request.range;
        self.unhandled(MACRO_CALL, r, why);
        let row = self.put_row(MacroCall { path: request.path, input: BodyRange::building(request.input), place: request.place }.encode(), r);
        match request.place {
            MacroPlace::Pat => {
                let p = self.put_pat(Pat::MacroCall { row }, r);
                self.push(V::Pat, p.0, r);
            }
            MacroPlace::Type => {
                let t = self.put_type(TypeRef::Macro(row), r);
                self.push(V::Type, t.0, r);
            }
            MacroPlace::Expr | MacroPlace::Stmt => {
                let e = self.put_expr(Expr::MacroCall { row }, r);
                self.push(V::Expr, e.0, r);
            }
            MacroPlace::Item => {}
        }
    }

    pub(crate) fn finish(mut self) -> Result<Lowered<'a, 't>, Need<'a>> {
        if let Some(grown) = &self.grown {
            self.out.expanded.extend_from_slice(&grown.as_bytes()[self.text.len()..]);
        }
        self.settle_patterns();
        if !self.needs.is_empty() {
            return Err(Need { atoms: self.needs });
        }
        if self.mode == Mode::AnonConst
            && let Some(v) = self.vals.iter().rev().find(|v| v.v == V::Expr)
        {
            self.out.head[head::ROOT] = v.id;
        }
        // A lone type's root: the last type made, as an anonymous constant's is its last
        // expression (each inner type's value is taken by its parent).
        if self.mode == Mode::Type
            && let Some(v) = self.vals.iter().rev().find(|v| v.v == V::Type)
        {
            self.out.head[head::TY] = v.id;
        }
        Ok(Lowered { columns: self.out, item: self.text })
    }

    /// Settles every identifier pattern by what its name is where it is written, innermost
    /// first: an item declared in one of the body's blocks, else the world's answer at the
    /// body's module ([`ValueNames`], each answer kept in the read list `value_reads`). The
    /// Reference, "Identifier patterns": "Path patterns take precedence over identifier
    /// patterns", so a bare name of a constant, a unit struct or a unit variant is a
    /// [`Pat::Path`], and each use its binding was given becomes that path; a binding of a
    /// static, a tuple struct or a tuple variant, or a `ref`, `mut` or `@` binding of any of
    /// those, is [`DiagKind::PatternShadows`] at the name (E0530). Run once, at the end, when
    /// every block's items are known; one pass over the patterns, one over what names a binding.
    fn settle_patterns(&mut self) {
        let Some(values) = self.values.as_deref_mut() else { return };
        let alloc = self.vals.allocator().clone();
        // Each pattern's settling: (pattern, name, block, answer).
        let mut settled: AVec<'_, (u32, u32, u32, ValueName)> = Vec::new_in(alloc.clone());
        let mut reads: AVec<'_, Node> = Vec::new_in(alloc.clone());
        {
            let v = self.out.view(self.text);
            for id in self.idents.iter() {
                let Ok(Pat::Bind { name, .. }) = v.pat(PatId(id.pat)) else {
                    unreachable!("an identifier pattern is the binding the lowering wrote");
                };
                // A `use` of a block around the pattern is asked first (`use E::*;` makes `A`
                // the variant): crate::block_uses, where the order's rule is quoted.
                let local = local_value(&v, ScopeId(id.scope), name);
                let asked = crate::block_uses::ask(&v, ScopeId(id.scope), name, local.is_some(), &mut *values, alloc.clone()).expect("the lowering's own scopes");
                if let Some(row) = crate::block_uses::read_row(name, ScopeId(id.scope), local.is_some(), asked) {
                    reads.push(row);
                }
                let by_use = match asked {
                    crate::block_uses::Asked::Bound(a) => Some(a),
                    _ => None,
                };
                let answer = match by_use.or(local) {
                    Some(a) => a,
                    None => {
                        let text = v.name_text(name).expect("a name the lowering wrote has its text");
                        let a = values.value_name(text);
                        reads.push(Node { a: name.0, ..Node::of(a as u16) });
                        a
                    }
                };
                if answer != ValueName::Nothing {
                    settled.push((id.pat, name.0, if id.bare { id.scope } else { NONE }, answer));
                }
            }
        }
        self.out.value_reads.extend_from_slice(&reads);
        // (pattern made a path, the binding it hid), in pattern order.
        let mut moved: AVec<'_, (u32, Option<PatId>)> = Vec::new_in(alloc);
        for &(pat, name, scope, answer) in settled.iter() {
            let at = self.out.name_ranges[name as usize];
            if answer == ValueName::Need {
                self.needs.push(at);
                continue;
            }
            if !answer.is_path() || scope == NONE {
                self.diag(DiagKind::PatternShadows, at);
                continue;
            }
            let seg = Segment {
                kind: SegKind::Ident,
                name: Some(NameIdx(name)),
                args: List { start: 0, len: 0 },
                has_args: false,
                parenthesized: false,
                return_notation: false,
                ty: None,
                trait_: None,
            };
            let mut node = seg.encode();
            node.f = 0;
            let segments = self.put_rows_of(&[(node, at)]);
            let path = self.put_path(PathRef { segments, absolute: false, scope: Some(ScopeId(scope)), lang: None }, at);
            let Ok(Pat::Bind { shadows, .. }) = Pat::decode(self.out.pats[pat as usize]) else {
                unreachable!("an identifier pattern is the binding the lowering wrote");
            };
            self.out.pats[pat as usize] = Pat::Path(path).encode();
            moved.push((pat, shadows));
        }
        if moved.is_empty() {
            return;
        }
        let find = |p: PatId| moved.binary_search_by_key(&p.0, |m| m.0).ok().map(|i| moved[i].1);
        for e in self.out.exprs.iter_mut() {
            if let Ok(Expr::Local { pat, path }) = Expr::decode(*e)
                && find(pat).is_some()
            {
                *e = Expr::Path(path).encode();
            }
        }
        for p in self.out.pats.iter_mut() {
            if let Ok(Pat::Bind { ann, name, sub, shadows: Some(s), ty }) = Pat::decode(*p)
                && let Some(hid) = find(s)
            {
                *p = Pat::Bind { ann, name, sub, shadows: hid, ty }.encode();
            }
        }
    }

    /// Sizes the value and frame stacks once for a parse about to be fed ([`crate::feed::Sizes`]),
    /// so no event of it grows them by reallocation (they grew by doubling from empty before).
    pub(crate) fn size_for(&mut self, sizes: crate::feed::Sizes) {
        self.vals.reserve(sizes.vals);
        self.frames.reserve(sizes.depth);
    }

    /// The value and frame stacks' capacities: the tests' measure of floor C3.
    #[cfg(test)]
    pub(crate) fn stack_capacities(&self) -> (usize, usize) {
        (self.vals.capacity(), self.frames.capacity())
    }

    // -----------------------------------------------------------------------------------------
    // Events

    pub(crate) fn enter(&mut self, kind: SyntaxKind) {
        if self.skip == 0 && !matches!(kind, ATTR | DOC_COMMENT) && self.frames.last().is_some_and(|f| f.flags & F_ATTRS != 0) {
            self.attrs_read();
        }
        if self.skip > 0 {
            self.skip += 1;
            if self.skip == 2 && kind == NAME && self.skip_name.start == NONE {
                self.skip_arm = true;
            }
            return;
        }
        if self.frames.is_empty() && self.root_depth == usize::MAX && self.mode != Mode::AnonConst {
            self.root_depth = if matches!(kind, SOURCE_FILE | MACRO_ITEMS) { 1 } else { 0 };
        }
        let skip = self.should_skip(kind);
        // An item of a block is skipped wholesale (its own lowering reads it), but its outer
        // attributes first, as a statement's: the Reference, "Conditional compilation"
        // [cfg.attr.behavior]: "If the predicate is false, the thing the attribute is attached to
        // is removed from the source code". A `#[cfg(..)] use libc::open64 as open;` false for
        // the target (the pinned std's `sanitize_standard_fds`) leaves no row of the block's
        // scope, so no reader (the block's imports and traits, its values) sees it.
        let attrs_first = skip && is_item(kind) && self.frames.last().is_some_and(|p| matches!(p.kind, STMT_LIST | MACRO_STMTS));
        if !skip {
            self.before_child(kind);
        }
        self.frames.push(Frame {
            kind,
            start: NONE,
            base: idx(self.vals.len()),
            locals: idx(self.locals.len()),
            pending: idx(self.pending.len()),
            saved_barrier: self.barrier,
            aux: NONE,
            flags: match (attrs_first, skip) {
                (true, _) => F_ATTRS,
                (false, true) => F_SKIP,
                (false, false) => 0,
            },
        });
        if attrs_first {
            return;
        }
        if skip {
            self.skip = 1;
            self.skip_name = NO_RANGE;
            self.skip_arm = false;
            return;
        }
        self.on_enter(kind);
    }

    pub(crate) fn token(&mut self, kind: SyntaxKind, range: Range) {
        for f in self.frames.iter_mut().rev() {
            if f.start != NONE {
                break;
            }
            f.start = range.start;
        }
        self.last_end = range.end;
        if self.skip == 0 && self.frames.last().is_some_and(|f| f.flags & F_ATTRS != 0) {
            self.attrs_read();
        }
        if self.skip > 0 {
            if self.skip_arm {
                self.skip_name = range;
                self.skip_arm = false;
            }
            return;
        }
        self.on_token(kind);
        self.vals.push(Val::of(V::Tok(kind), NONE, range));
    }

    pub(crate) fn exit(&mut self) {
        if self.skip > 0 {
            self.skip -= 1;
            if self.skip > 0 {
                return;
            }
            let top = *self.frames.last().expect("a skipped node's exit has its frame");
            if top.flags & F_DEAD == 0 {
                self.frames.pop();
                let range = self.range_of(&top);
                let mut node = Node::of(0);
                node.a = self.skip_name.start;
                node.b = self.skip_name.end;
                self.vals.push(Val { v: V::Skipped(top.kind), id: NONE, node, range });
                // A `macro` item is in scope in its whole block (path-based scope, the
                // Reference's "Macros By Example", Scoping), a `macro_rules!` from here on; the
                // events come in text order, so a call written above a `macro` of its block is
                // resolved as if the block had none (a gap: it needs the block's items first).
                if matches!(top.kind, MACRO_RULES | MACRO_DEF) && self.skip_name.start != NONE {
                    let depth = idx(self.scopes.len());
                    let (locals, entries) = (idx(self.locals.len()), idx(self.entries.len()));
                    let macro2 = top.kind == MACRO_DEF;
                    self.local_macros.push(LocalMacroAt { name: self.skip_name, item: range, depth, locals, entries, macro2 });
                }
                self.after_child();
                return;
            }
        }
        let f = self.frames.pop().expect("an exit has its enter");
        let range = self.range_of(&f);
        if f.flags & F_DEAD == 0 {
            self.lower(&f, range);
        } else {
            self.vals.truncate(f.base as usize);
            if self.frames.len() == self.root_depth {
                self.out.head[head::CFG_OUT] = 1;
            }
        }
        self.cleanup(&f);
        self.after_child();
    }

    /// A lazy node of an outline: a fn's body, never parsed.
    pub(crate) fn lazy(&mut self, kind: SyntaxKind, range: Range) {
        for f in self.frames.iter_mut().rev() {
            if f.start != NONE {
                break;
            }
            f.start = range.start;
        }
        self.last_end = range.end;
        if self.skip == 0 && self.frames.last().is_some_and(|f| f.flags & F_ATTRS != 0) {
            self.attrs_read();
        }
        if self.skip > 0 {
            return;
        }
        let mut val = Val::of(V::Skipped(kind), NONE, range);
        val.node.a = NONE;
        val.node.b = NONE;
        self.vals.push(val);
    }

    pub(crate) fn parse_error(&mut self) {
        if self.skip > 0 {
            // Inside a node skipped wholesale: its own lowering reports it (and a signature's
            // record stays a function of its key's text, which a body is not part of).
            return;
        }
        let at = self.last_end;
        self.out.diags.push(Diag { kind: DiagKind::Syntax, range: BodyRange::building(Range::new(at, at)) }.encode());
    }

    fn range_of(&self, f: &Frame) -> Range {
        let start = if f.start == NONE { self.last_end } else { f.start };
        Range::new(start, self.last_end.max(start))
    }

    fn cleanup(&mut self, f: &Frame) {
        if f.flags & F_LOCALS != 0 {
            self.pop_locals(f.locals);
        }
        if f.flags & F_BARRIER != 0 {
            self.barrier = f.saved_barrier;
        }
        if f.flags & F_LOOP != 0 {
            let target = core::mem::replace(&mut self.loop_target, NONE);
            self.pop_entry(target);
        }
        if f.flags & F_CORO != 0 {
            self.coros.pop();
        }
        if f.flags & F_SCOPE != 0 {
            self.scopes.pop();
            // The block's own `macro_rules!` go out of scope with it.
            while self.local_macros.last().is_some_and(|l| l.depth as usize > self.scopes.len()) {
                self.local_macros.pop();
            }
        }
        if is_binder(f.kind) {
            self.pending.truncate(f.pending as usize);
        }
    }

    fn top_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a node's value has its parent open")
    }

    fn after_child(&mut self) {
        let Some(p) = self.frames.last().copied() else { return };
        let Some(last) = self.vals.last().copied() else { return };
        if (last.v == V::CfgFalse) && !matches!(p.kind, ATTR | CFG_ATTR_META | UNSAFE_META) && p.flags & F_SKIP == 0 {
            self.vals.truncate(p.base as usize);
            self.top_mut().flags |= F_DEAD;
            self.skip = 1;
            return;
        }
        match p.kind {
            MATCH_ARM if p.flags & F_ACTIVE == 0 && last.v == V::Pat => {
                self.activate(p.pending);
                self.top_mut().flags |= F_ACTIVE;
            }
            IF_EXPR if p.flags & F_POPPED == 0 && self.count(&p, |v| v == V::Expr) == 2 => {
                // The condition's `let` bindings are out of scope in the `else`.
                self.pop_locals(p.locals);
                self.top_mut().flags |= F_POPPED;
            }
            _ => {}
        }
    }

    fn before_child(&mut self, kind: SyntaxKind) {
        let depth = self.frames.len();
        let Some(p) = self.frames.last().copied() else { return };
        match p.kind {
            FOR_EXPR if p.flags & F_FOR == 0 && self.count(&p, |v| matches!(v, V::Pat | V::Expr)) == 2 => {
                // The body: the pattern's bindings come into scope, and the loop opens.
                self.activate(p.pending);
                if p.flags & F_INLINE != 0 {
                    self.top_mut().flags |= F_FOR;
                } else {
                    let label = match p.aux {
                        NONE => 0,
                        name => self.local_key(NameIdx(name)),
                    };
                    self.push_entry(Jk::Loop, label);
                    self.top_mut().flags |= F_FOR | F_LOOP;
                }
            }
            ARRAY_EXPR if p.flags & F_SEMI != 0 => {
                // A repeat's length is an anonymous constant: no local of the body.
                let barrier = idx(self.locals.len());
                let saved = self.barrier;
                self.barrier = barrier;
                self.push_entry(Jk::Barrier, 0);
                let top = self.top_mut();
                top.saved_barrier = saved;
                top.flags = (top.flags & !F_SEMI) | F_BARRIER | F_LOOP;
            }
            BLOCK_EXPR if kind == STMT_LIST => {
                if p.flags & (F_ASYNC | F_GEN) != 0 {
                    self.coros.push(p.flags & (F_ASYNC | F_GEN));
                    self.push_entry(Jk::Barrier, 0);
                    self.top_mut().flags |= F_CORO | F_LOOP;
                } else if p.flags & F_CONST != 0 {
                    let saved = self.barrier;
                    self.barrier = idx(self.locals.len());
                    self.push_entry(Jk::Barrier, 0);
                    let top = self.top_mut();
                    top.saved_barrier = saved;
                    top.flags |= F_BARRIER | F_LOOP;
                } else if p.flags & F_TRY != 0 && p.flags & F_LOOP == 0 {
                    self.push_entry(Jk::Try, 0);
                    self.top_mut().flags |= F_LOOP;
                }
            }
            FN if depth == self.root_depth.wrapping_add(1)
                && self.mode == Mode::Body
                && kind == BLOCK_EXPR
                && p.flags & F_ASYNC != 0 =>
            {
                self.coros.push(F_ASYNC);
                self.top_mut().flags |= F_CORO;
            }
            _ => {}
        }
    }

    fn on_enter(&mut self, kind: SyntaxKind) {
        let depth = self.frames.len() - 1;
        match kind {
            STMT_LIST => {
                let parent = self.scopes.last().copied();
                let id = idx(self.out.scopes.len());
                self.out
                    .scopes
                    .push(Scope { parent: parent.map(ScopeId), items: List { start: 0, len: 0 } }.encode());
                self.scopes.push(id);
                self.top_mut().flags |= F_SCOPE | F_LOCALS;
            }
            MATCH_ARM | IF_EXPR | FOR_EXPR => self.top_mut().flags |= F_LOCALS,
            LOOP_EXPR | WHILE_EXPR => {
                self.push_entry(Jk::Loop, 0);
                self.top_mut().flags |= F_LOOP | F_LOCALS;
            }
            CLOSURE_EXPR => {
                self.push_entry(Jk::Barrier, 0);
                self.top_mut().flags |= F_LOOP | F_LOCALS;
            }
            FN if depth == self.root_depth && self.mode == Mode::Body => self.top_mut().flags |= F_LOCALS,
            _ => {}
        }
    }

    fn on_token(&mut self, kind: SyntaxKind) {
        let depth = self.frames.len();
        let mode = self.mode;
        let root = self.root_depth.wrapping_add(1);
        let Some(top) = self.frames.last_mut() else { return };
        match (top.kind, kind) {
            (VARIANT | RECORD_FIELD | KEY_VALUE_META, EQ) => top.flags |= F_EQ,
            (CONST | STATIC, EQ) if depth == root && mode == Mode::Signature => top.flags |= F_EQ,
            (BLOCK_EXPR | CLOSURE_EXPR, ASYNC_KW) => top.flags |= F_ASYNC,
            (BLOCK_EXPR | CLOSURE_EXPR, GEN_KW) => top.flags |= F_GEN,
            (BLOCK_EXPR | CLOSURE_EXPR, CONST_KW) => top.flags |= F_CONST,
            (BLOCK_EXPR, UNSAFE_KW) => top.flags |= F_UNSAFE,
            (BLOCK_EXPR | CLOSURE_EXPR, MOVE_KW) => top.flags |= F_MOVE,
            (CLOSURE_EXPR, STATIC_KW) => top.flags |= F_STATIC,
            (FN, ASYNC_KW) if depth == root => top.flags |= F_ASYNC,
            (FN, GEN_KW) if depth == root => top.flags |= F_GEN,
            (ARRAY_EXPR, SEMICOLON) => top.flags |= F_SEMI,
            (FOR_EXPR, INLINE_KW) => top.flags |= F_INLINE,
            _ => {}
        }
    }

    fn should_skip(&self, kind: SyntaxKind) -> bool {
        if matches!(kind, TOKEN_TREE | CONST_ARG | USE_TREE | ITEM_LIST | EXTERN_ITEM_LIST | DOC_COMMENT) {
            return true;
        }
        let Some(p) = self.frames.last() else { return false };
        if p.flags & F_EQ != 0 && matches!(p.kind, VARIANT | RECORD_FIELD | KEY_VALUE_META) {
            return true;
        }
        if matches!(p.kind, STMT_LIST | MACRO_STMTS) && is_item(kind)
            || p.kind == ASSOC_ITEM_LIST && (is_item(kind) || kind == MACRO_CALL)
        {
            return true;
        }
        // A fn pointer type's parameter name (`fn(_: u8)`, `fn(x: u8)`; the Reference, "Function
        // pointer types", `MaybeNamedParam`) is no pattern and binds nothing: never a pattern
        // of the body (quickcheck_macros' `p as fn(_: T) -> bool` left PatId(0) untyped). The
        // `PARAM` then reads no `V::Pat` (its `pat` is `None`), as a fn pointer's unnamed one.
        // The nodes nest FN_PTR_TYPE > PARAM_LIST > PARAM > pattern (parser params.rs
        // `Flavor::FnPointer`).
        if p.kind == PARAM
            && matches!(kind, WILDCARD_PAT | IDENT_PAT)
            && self.frames.len().checked_sub(3).and_then(|i| self.frames.get(i)).is_some_and(|g| g.kind == FN_PTR_TYPE)
        {
            return true;
        }
        let depth = self.frames.len();
        let parent_is_root = depth == self.root_depth.wrapping_add(1);
        match self.mode {
            Mode::Signature => {
                parent_is_root && (kind == BLOCK_EXPR || p.flags & F_EQ != 0 && matches!(p.kind, CONST | STATIC))
                    // A `where const { .. }` condition is an anonymous constant, lowered on its own.
                    || p.kind == WHERE_PRED && kind == BLOCK_EXPR
            }
            Mode::Body => {
                if parent_is_root {
                    // An async fn's body is checked against its written return type: kept.
                    if kind == RET_TYPE && p.kind == FN && p.flags & F_ASYNC != 0 {
                        return false;
                    }
                    return matches!(kind, GENERIC_PARAM_LIST | WHERE_CLAUSE | RET_TYPE | VISIBILITY | ABI)
                        || matches!(p.kind, CONST | STATIC) && is_type(kind);
                }
                depth == self.root_depth.wrapping_add(3)
                    && matches!(p.kind, PARAM | SELF_PARAM)
                    && is_type(kind)
                    && self.frames.get(self.root_depth).is_some_and(|r| r.kind == FN)
            }
            Mode::AnonConst | Mode::Type => false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Locals, entries, scopes

    fn activate(&mut self, from: u32) {
        let start = from as usize;
        let mut i = start;
        while i < self.pending.len() {
            let (name, pat) = self.pending[i];
            // An or-pattern binds a name once per alternative: one binding, the first's.
            if !self.pending[start..i].iter().any(|p| p.0 == name) {
                self.push_local(name, pat);
            }
            i += 1;
        }
        self.pending.truncate(start);
    }

    fn push_local(&mut self, name: u64, pat: u32) {
        let prev = self.top.get(&name).copied().unwrap_or(NONE);
        if prev != NONE {
            let hidden = self.locals[prev as usize].pat;
            self.out.pats[pat as usize].c = hidden;
        }
        let at = idx(self.locals.len());
        self.locals.push(Local { name, pat, prev });
        self.top.insert(name, at);
    }

    fn pop_locals(&mut self, to: u32) {
        while self.locals.len() > to as usize {
            let l = self.locals.pop().expect("a local above the mark");
            if l.prev == NONE {
                self.top.remove(&l.name);
            } else {
                self.top.insert(l.name, l.prev);
            }
        }
    }

    pub(crate) fn push_pending(&mut self, name: u64, pat: PatId) {
        self.pending.push((name, pat.0));
    }

    fn push_entry(&mut self, kind: Jk, label: u64) {
        let jumps = idx(self.jumps.len());
        self.entries.push(Entry { kind, label, jumps });
    }

    fn pop_entry(&mut self, target: u32) {
        let Some(e) = self.entries.pop() else { return };
        let me = idx(self.entries.len());
        let mut keep = e.jumps as usize;
        for i in e.jumps as usize..self.jumps.len() {
            let (entry, expr) = self.jumps[i];
            if entry == me {
                if target != NONE {
                    self.out.exprs[expr as usize].b = target;
                }
            } else {
                self.jumps[keep] = self.jumps[i];
                keep += 1;
            }
        }
        self.jumps.truncate(keep);
    }

    /// The entry a `break` (`want_loop` false) or `continue` (true) with `label` leaves.
    /// The entry a `break`/`continue` of label key `label` (0: none) leaves; a label is
    /// matched only among the first `below` entries (def_site.rs `find_label`).
    fn find_entry(&self, label: u64, want_loop: bool, below: usize) -> Result<usize, DiagKind> {
        for i in (0..self.entries.len()).rev() {
            let e = self.entries[i];
            match e.kind {
                Jk::Barrier => break,
                _ if label != 0 => {
                    if e.label == label && i < below {
                        return match (e.kind, want_loop) {
                            (Jk::Block, true) => Err(DiagKind::ContinueToBlock),
                            _ => Ok(i),
                        };
                    }
                }
                Jk::Loop => return Ok(i),
                Jk::Block | Jk::Try => {}
            }
        }
        Err(if label != 0 { DiagKind::UndeclaredLabel } else { DiagKind::BreakOutsideLoop })
    }

    /// The innermost `try` block a `?` breaks to, inside the closest barrier.
    pub(crate) fn try_entry(&self) -> Option<usize> {
        for i in (0..self.entries.len()).rev() {
            match self.entries[i].kind {
                Jk::Barrier => return None,
                Jk::Try => return Some(i),
                _ => {}
            }
        }
        None
    }

    /// Inside an `async` or `gen` body, with no plain closure between.
    pub(crate) fn in_coroutine(&self) -> bool {
        self.coros.last().is_some_and(|c| *c != NOT_CORO)
    }

    /// Inside an `async` body (an `async gen` one too), with no plain closure between.
    pub(crate) fn in_async(&self) -> bool {
        self.coros.last().is_some_and(|c| *c != NOT_CORO && c & F_ASYNC != 0)
    }

    pub(crate) fn add_jump(&mut self, entry: usize, expr: ExprId) {
        self.jumps.push((idx(entry), expr.0));
    }

    pub(crate) fn scope(&self) -> ScopeId {
        ScopeId(self.scopes.last().copied().unwrap_or(0))
    }

    // -----------------------------------------------------------------------------------------
    // Writing

    pub(crate) fn put_expr(&mut self, e: Expr, r: Range) -> ExprId {
        let id = idx(self.out.exprs.len());
        self.out.exprs.push(e.encode());
        self.out.expr_ranges.push(r);
        ExprId(id)
    }

    pub(crate) fn put_pat(&mut self, p: Pat, r: Range) -> PatId {
        let id = idx(self.out.pats.len());
        self.out.pats.push(p.encode());
        self.out.pat_ranges.push(r);
        PatId(id)
    }

    pub(crate) fn put_stmt(&mut self, s: Stmt) -> StmtId {
        let id = idx(self.out.stmts.len());
        self.out.stmts.push(s.encode());
        StmtId(id)
    }

    pub(crate) fn put_type(&mut self, t: TypeRef, r: Range) -> TypeId {
        let id = idx(self.out.types.len());
        self.out.types.push(t.encode());
        self.out.type_ranges.push(r);
        TypeId(id)
    }

    pub(crate) fn put_path(&mut self, p: PathRef, r: Range) -> PathIdx {
        let id = idx(self.out.paths.len());
        self.out.paths.reserve(1);
        self.out.path_ranges.reserve(1);
        self.out.paths.push(p.encode());
        self.out.path_ranges.push(r);
        PathIdx(id)
    }

    pub(crate) fn put_row(&mut self, n: Node, r: Range) -> RowIdx {
        let id = idx(self.out.rows.len());
        self.out.rows.push(n);
        self.out.row_ranges.push(r);
        RowIdx(id)
    }

    pub(crate) fn put_rows_of(&mut self, nodes: &[(Node, Range)]) -> List {
        let start = idx(self.out.rows.len());
        // One growth for the list, not one per doubling the rows pass on the way.
        self.out.rows.reserve(nodes.len());
        self.out.row_ranges.reserve(nodes.len());
        for (n, r) in nodes {
            self.out.rows.push(*n);
            self.out.row_ranges.push(*r);
        }
        List { start, len: idx(nodes.len()) }
    }

    pub(crate) fn put_ids_of(&mut self, ids: &[u32]) -> List {
        let start = idx(self.out.ids.len());
        self.out.ids.extend_from_slice(ids);
        List { start, len: idx(ids.len()) }
    }

    /// The ids of the children of `f` of kind `v`, as a list of `ids`.
    fn put_ids(&mut self, f: &Frame, v: V) -> List {
        let start = idx(self.out.ids.len());
        for i in f.base as usize..self.vals.len() {
            if self.vals[i].v == v {
                self.out.ids.push(self.vals[i].id);
            }
        }
        List { start, len: idx(self.out.ids.len()) - start }
    }

    /// The unplaced rows of `tag` among the children of `f`, placed.
    fn put_rows(&mut self, f: &Frame, tag: u16) -> List {
        let start = idx(self.out.rows.len());
        let count = self.vals[f.base as usize..].iter().filter(|v| v.v == V::Row(tag)).count();
        self.out.rows.reserve(count);
        self.out.row_ranges.reserve(count);
        for i in f.base as usize..self.vals.len() {
            let v = self.vals[i];
            if v.v == V::Row(tag) {
                self.out.rows.push(v.node);
                self.out.row_ranges.push(v.range);
            }
        }
        List { start, len: idx(self.out.rows.len()) - start }
    }

    pub(crate) fn name(&mut self, r: Range) -> NameIdx {
        let text = r.of(self.text());
        let text = text.strip_prefix("r#").unwrap_or(text);
        let id = idx(self.out.names.len());
        self.out.names.push(Name::of(text).0);
        self.out.name_ranges.push(r);
        NameIdx(id)
    }

    pub(crate) fn synth_name(&mut self, text: &str, r: Range) -> NameIdx {
        let id = idx(self.out.names.len());
        self.out.names.push(Name::of(text).0);
        self.out.name_ranges.push(Range::new(r.start, r.start));
        NameIdx(id)
    }

    pub(crate) fn unhandled(&mut self, kind: SyntaxKind, r: Range, why: Why) {
        self.out.unhandled.push(Unhandled { syntax: kind as u16, range: BodyRange::building(r), why }.encode());
    }

    pub(crate) fn diag(&mut self, kind: DiagKind, r: Range) {
        self.out.diags.push(Diag { kind, range: BodyRange::building(r) }.encode());
    }

    fn push(&mut self, v: V, id: u32, r: Range) {
        self.vals.push(Val::of(v, id, r));
    }

    fn push_node(&mut self, v: V, node: Node, r: Range) {
        self.vals.push(Val { v, id: NONE, node, range: r });
    }

    fn done(&mut self, f: &Frame) {
        self.vals.truncate(f.base as usize);
    }

    // -----------------------------------------------------------------------------------------
    // Reading a node's children

    fn kid(&self, f: &Frame, n: usize, pred: impl Fn(V) -> bool) -> Option<Val> {
        self.vals[f.base as usize..].iter().filter(|v| pred(v.v)).nth(n).copied()
    }

    fn first(&self, f: &Frame, v: V) -> Option<Val> {
        self.kid(f, 0, |x| x == v)
    }

    fn has_tok(&self, f: &Frame, k: SyntaxKind) -> bool {
        self.vals[f.base as usize..].iter().any(|v| v.v == V::Tok(k))
    }

    fn count(&self, f: &Frame, pred: impl Fn(V) -> bool) -> usize {
        self.vals[f.base as usize..].iter().filter(|v| pred(v.v)).count()
    }

    fn list_of(&self, f: &Frame, l: L) -> Option<(List, Val)> {
        self.first(f, V::List(l)).map(|v| (List { start: v.id, len: v.node.a }, v))
    }

    fn parent_kind(&self) -> Option<SyntaxKind> {
        self.frames.last().map(|f| f.kind)
    }

    fn is_root_frame(&self) -> bool {
        self.frames.len() == self.root_depth
    }

    // -----------------------------------------------------------------------------------------
    // Handlers

    fn lower(&mut self, f: &Frame, r: Range) {
        match f.kind {
            NAME => {
                let t = self.kid(f, 0, |v| matches!(v, V::Tok(_))).map_or(Range::new(r.start, r.start), |v| v.range);
                self.done(f);
                let n = self.name(t);
                self.push(V::Name, n.0, r);
            }
            NAME_REF => {
                let t = self.kid(f, 0, |v| matches!(v, V::Tok(_)));
                self.done(f);
                let (kind, range) = t.map_or((ERROR, Range::new(r.start, r.start)), |v| match v.v {
                    V::Tok(k) => (k, v.range),
                    _ => (ERROR, v.range),
                });
                let n = self.name(range);
                self.push(V::NameRef(kind), n.0, r);
            }
            LIFETIME => {
                let t = self.kid(f, 0, |v| matches!(v, V::Tok(_))).map_or(r, |v| v.range);
                self.done(f);
                let n = self.name(t);
                self.push(V::Lifetime, n.0, r);
            }
            LITERAL => self.literal(f, r),
            PATH_SEGMENT => self.path_segment(f, r),
            TYPE_ANCHOR => {
                let ty = self.kid(f, 0, |v| v == V::Type).map(|v| TypeId(v.id));
                let tr = self.kid(f, 1, |v| v == V::Type).map(|v| TypeId(v.id));
                self.done(f);
                let seg = Segment {
                    kind: SegKind::Anchor,
                    name: None,
                    args: List { start: 0, len: 0 },
                    has_args: false,
                    parenthesized: false,
                    return_notation: false,
                    ty,
                    trait_: tr,
                };
                let mut node = seg.encode();
                node.f = 0;
                self.push_node(V::Seg, node, r);
            }
            PATH => self.path(f, r),
            GENERIC_ARG_LIST => self.generic_args(f, r),
            PARENTHESIZED_ARG_LIST => {
                let mut nodes: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    match v.v {
                        V::Type => nodes.push((GenericArg::Type(TypeId(v.id)).encode(), v.range)),
                        V::Row(ROW_ARG) => nodes.push((v.node, v.range)),
                        _ => {}
                    }
                }
                self.done(f);
                let l = self.put_rows_of(&nodes);
                let mut node = Node::of(0);
                node.a = l.len;
                node.b = 1;
                self.vals.push(Val { v: V::List(L::GArgs), id: l.start, node, range: r });
            }
            RETURN_TYPE_SYNTAX => {
                self.done(f);
                let mut node = Node::of(0);
                node.a = 0;
                node.b = 2;
                self.vals.push(Val { v: V::List(L::GArgs), id: idx(self.out.rows.len()), node, range: r });
            }
            RET_TYPE => {
                let t = self.first(f, V::Type);
                self.done(f);
                if let Some(t) = t {
                    self.push(V::Ret, t.id, r);
                }
            }
            TYPE_ARG => {
                let t = self.first(f, V::Type);
                self.done(f);
                let t = t.map_or_else(|| self.put_type(TypeRef::Error, r), |v| TypeId(v.id));
                self.push_node(V::Row(ROW_ARG), GenericArg::Type(t).encode(), r);
            }
            LIFETIME_ARG => {
                let l = self.first(f, V::Lifetime);
                self.done(f);
                if let Some(l) = l {
                    self.push_node(V::Row(ROW_ARG), GenericArg::Lifetime(NameIdx(l.id)).encode(), r);
                }
            }
            ASSOC_TYPE_ARG => self.assoc_type_arg(f, r),
            // Types.
            PATH_TYPE => {
                let p = self.first(f, V::Path);
                self.done(f);
                let t = match p {
                    Some(p) => TypeRef::Path(PathIdx(p.id)),
                    None => TypeRef::Error,
                };
                let t = self.put_type(t, r);
                self.push(V::Type, t.0, r);
            }
            REF_TYPE => {
                let lifetime = self.first(f, V::Lifetime).map(|v| NameIdx(v.id));
                let mutbl = if self.has_tok(f, MUT_KW) || self.has_tok(f, UNIQ_KW) { Mutbl::Mut } else { Mutbl::Not };
                let inner = self.first(f, V::Type);
                self.done(f);
                self.type_of(inner, r, |inner| TypeRef::Ref { mutbl, lifetime, inner });
            }
            PTR_TYPE => {
                let mutbl = if self.has_tok(f, MUT_KW) { Mutbl::Mut } else { Mutbl::Not };
                let inner = self.first(f, V::Type);
                self.done(f);
                self.type_of(inner, r, |inner| TypeRef::Ptr { mutbl, inner });
            }
            TUPLE_TYPE => {
                let l = self.put_ids(f, V::Type);
                self.done(f);
                let t = self.put_type(TypeRef::Tuple(l), r);
                self.push(V::Type, t.0, r);
            }
            PAREN_TYPE => {
                // `(T)` is `T`.
                let inner = self.first(f, V::Type);
                self.done(f);
                match inner {
                    Some(inner) => self.push(V::Type, inner.id, r),
                    None => self.type_of(None, r, |_| TypeRef::Error),
                }
            }
            ARRAY_TYPE => {
                let elem = self.first(f, V::Type);
                let len = self.first(f, V::Skipped(CONST_ARG));
                self.done(f);
                let t = match (elem, len) {
                    (Some(elem), Some(len)) => {
                        let c = self.put_const(len.range);
                        TypeRef::Array { elem: TypeId(elem.id), len: c }
                    }
                    _ => TypeRef::Error,
                };
                let t = self.put_type(t, r);
                self.push(V::Type, t.0, r);
            }
            SLICE_TYPE => {
                let inner = self.first(f, V::Type);
                self.done(f);
                self.type_of(inner, r, TypeRef::Slice);
            }
            FN_PTR_TYPE => {
                let abi = self.first(f, V::Abi);
                let unsafety = self.has_tok(f, UNSAFE_KW);
                let params = self.list_of(f, L::Params);
                let ret = self.first(f, V::Ret).map(|v| TypeId(v.id));
                self.done(f);
                let params = params.map_or(List { start: 0, len: 0 }, |p| p.0);
                let variadic = self.variadic(params);
                let t = TypeRef::FnPtr {
                    params,
                    ret,
                    abi: abi.and_then(|a| (a.node.a == 1).then_some(BodyRange::building(a.range))),
                    unsafety,
                    extern_: abi.is_some(),
                    variadic,
                };
                let t = self.put_type(t, r);
                self.push(V::Type, t.0, r);
            }
            FOR_TYPE => {
                let binder = self.list_of(f, L::GParams).map_or(List { start: 0, len: 0 }, |l| l.0);
                let inner = self.first(f, V::Type);
                self.done(f);
                self.type_of(inner, r, |inner| TypeRef::For { binder, inner });
            }
            IMPL_TRAIT_TYPE | DYN_TRAIT_TYPE => {
                let bounds = self.list_of(f, L::Bounds).map_or(List { start: 0, len: 0 }, |l| l.0);
                self.done(f);
                let t = if f.kind == IMPL_TRAIT_TYPE { TypeRef::Impl(bounds) } else { TypeRef::Dyn(bounds) };
                let t = self.put_type(t, r);
                self.push(V::Type, t.0, r);
            }
            NEVER_TYPE | INFER_TYPE => {
                self.done(f);
                let t = self.put_type(if f.kind == NEVER_TYPE { TypeRef::Never } else { TypeRef::Infer }, r);
                self.push(V::Type, t.0, r);
            }
            MACRO_TYPE | MACRO_EXPR | MACRO_PAT | MACRO_STMTS => {
                // The macro call left the value of the place it is in: unexpanded, its node;
                // expanded, its expansion's, lowered in place (statements stay in their
                // block's values, each one of its statements).
            }
            // `builtin # pattern_type(base is pat)`: core's `pattern_type!` expanded.
            PATTERN_TYPE => {
                let base = self.first(f, V::Type).map(|v| TypeId(v.id));
                let pat = if self.has_tok(f, NOT_NULL) { Some(TyPat::NotNull) } else { self.first(f, V::Pat).map(|v| TyPat::Of(PatId(v.id))) };
                self.done(f);
                let t = match (base, pat) {
                    (Some(base), Some(pat)) => TypeRef::Pat { base, pat },
                    _ => {
                        self.unhandled(f.kind, r, Why::Kind);
                        TypeRef::Error
                    }
                };
                let t = self.put_type(t, r);
                self.push(V::Type, t.0, r);
            }
            RECORD_TYPE => {
                let fields = self.list_of(f, L::Fields(Shape::Record)).map_or(List { start: 0, len: 0 }, |l| l.0);
                self.done(f);
                let t = self.put_type(TypeRef::Record(fields), r);
                self.push(V::Type, t.0, r);
            }
            UNION_TYPE => {
                let members = self.put_ids(f, V::Type);
                self.done(f);
                let t = self.put_type(TypeRef::Union(members), r);
                self.push(V::Type, t.0, r);
            }
            UNSAFE_BINDER_TYPE => {
                self.done(f);
                self.unhandled(f.kind, r, Why::Kind);
                let t = self.put_type(TypeRef::Error, r);
                self.push(V::Type, t.0, r);
            }
            ABI => {
                let s = self.kid(f, 0, |v| matches!(v, V::Tok(STRING)));
                self.done(f);
                let mut node = Node::of(0);
                node.a = u32::from(s.is_some());
                self.vals.push(Val { v: V::Abi, id: NONE, node, range: s.map_or(r, |s| s.range) });
            }
            // Bounds and generics.
            TYPE_BOUND_LIST => {
                let l = self.put_rows(f, ROW_BOUND);
                self.done(f);
                self.push_list(L::Bounds, l, r);
            }
            TYPE_BOUND => self.type_bound(f, r),
            USE_BOUND_GENERIC_ARGS => {
                let start = idx(self.out.ids.len());
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    if matches!(v.v, V::Lifetime | V::NameRef(_)) {
                        self.out.ids.push(v.id);
                    }
                }
                let l = List { start, len: idx(self.out.ids.len()) - start };
                self.done(f);
                self.push_list(L::Names, l, r);
            }
            // Read through: their children's values are their parent's.
            VISIBILITY_INNER => {}
            FOR_BINDER => {
                if self.has_tok(f, IN_KW) {
                    let param = self.first(f, V::Name);
                    let kind = self.first(f, V::Tok(IDENT));
                    let container = self.first(f, V::Type);
                    let is_fields = kind.is_some_and(|k| k.range.of(self.text()) == "fields");
                    self.done(f);
                    if is_fields && let (Some(param), Some(container)) = (param, container) {
                        let mut node = Node::of(0);
                        node.a = container.id;
                        self.vals.push(Val { v: V::ForFields, id: param.id, node, range: r });
                    } else {
                        self.diag(DiagKind::Syntax, r);
                    }
                }
            }
            GENERIC_PARAM_LIST => {
                let l = self.put_rows(f, ROW_GENERIC_PARAM);
                self.done(f);
                self.push_list(L::GParams, l, r);
            }
            LIFETIME_PARAM => {
                let name = self.first(f, V::Lifetime);
                let mut nodes: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    if v.v == V::Lifetime && Some(v.id) != name.map(|n| n.id) {
                        nodes.push((Bound::Lifetime(NameIdx(v.id)).encode(), v.range));
                    }
                }
                let listed = self.list_of(f, L::Bounds).map(|l| l.0);
                self.done(f);
                let bounds = match listed {
                    Some(l) => l,
                    None => self.put_rows_of(&nodes),
                };
                if let Some(name) = name {
                    let p = GenericParam::Lifetime { name: NameIdx(name.id), bounds };
                    self.push_node(V::Row(ROW_GENERIC_PARAM), p.encode(), r);
                }
            }
            TYPE_PARAM => {
                let name = self.first(f, V::Name);
                let bounds = self.list_of(f, L::Bounds).map_or(List { start: 0, len: 0 }, |l| l.0);
                let default = self.first(f, V::Type).map(|v| TypeId(v.id));
                self.done(f);
                if let Some(name) = name {
                    let p = GenericParam::Type { name: NameIdx(name.id), bounds, default };
                    self.push_node(V::Row(ROW_GENERIC_PARAM), p.encode(), r);
                }
            }
            CONST_PARAM => {
                let name = self.first(f, V::Name);
                let ty = self.first(f, V::Type);
                let default = self.first(f, V::Skipped(CONST_ARG));
                self.done(f);
                let ty = ty.map_or_else(|| self.put_type(TypeRef::Error, r), |v| TypeId(v.id));
                let default = default.map(|d| self.put_const(d.range));
                if let Some(name) = name {
                    let p = GenericParam::Const { name: NameIdx(name.id), ty, default };
                    self.push_node(V::Row(ROW_GENERIC_PARAM), p.encode(), r);
                }
            }
            WHERE_CLAUSE => {
                let l = self.put_rows(f, ROW_WHERE);
                self.done(f);
                self.push_list(L::Where, l, r);
            }
            WHERE_PRED => {
                let for_fields = self.first(f, V::ForFields);
                let binder = self.list_of(f, L::GParams).map_or(List { start: 0, len: 0 }, |l| l.0);
                let ty = self.first(f, V::Type);
                let lifetime = self.first(f, V::Lifetime);
                let bounds = self.list_of(f, L::Bounds).map_or(List { start: 0, len: 0 }, |l| l.0);
                let cond = self.first(f, V::Skipped(BLOCK_EXPR));
                self.done(f);
                let p = match (for_fields, ty, lifetime) {
                    (Some(ff), Some(t), _) => WherePred::ForFields {
                        param: NameIdx(ff.id),
                        container: TypeId(ff.node.a),
                        subject: TypeId(t.id),
                        bounds,
                    },
                    (None, Some(t), _) => WherePred::Type { binder, ty: TypeId(t.id), bounds },
                    (None, None, Some(l)) => WherePred::Lifetime { name: NameIdx(l.id), bounds },
                    (None, None, None) => match cond {
                        Some(c) => WherePred::Const { cond: self.put_const(c.range) },
                        None => return,
                    },
                    (Some(_), None, _) => return,
                };
                self.push_node(V::Row(ROW_WHERE), p.encode(), r);
            }
            // `name = expr` or `expr`, an argument of `builtin # format_args(..)`.
            FORMAT_ARGS_ARG => {
                let name = self.first(f, V::Name).map(|v| NameIdx(v.id));
                let expr = self.first(f, V::Expr);
                self.done(f);
                let expr = expr.map_or_else(|| self.put_expr(Expr::Missing, r), |v| ExprId(v.id));
                self.push_node(V::Row(ROW_FMT_ARG), FmtArg { name, expr, captured: false }.encode(), r);
            }
            // Parameters.
            PARAM_LIST => self.param_list(f, r),
            PARAM => {
                let attrs = self.put_rows(f, ROW_ATTR);
                let pat = self.first(f, V::Pat).map(|v| PatId(v.id));
                let ty = self.first(f, V::Type).map(|v| TypeId(v.id));
                let variadic = self.has_tok(f, DOT3);
                self.done(f);
                let p = Param { pat, ty, self_kind: SelfKind::No, lifetime: None, variadic, attrs };
                self.push_node(V::Row(ROW_PARAM), p.encode(), r);
            }
            SELF_PARAM => self.self_param(f, r),
            // Attributes.
            ATTR => self.attr(f, r),
            PATH_META | KEY_VALUE_META | TOKEN_TREE_META => {
                let path = self.first(f, V::Path);
                let args = self.kid(f, 0, |v| matches!(v, V::Skipped(_)));
                self.done(f);
                let meta = match f.kind {
                    PATH_META => AttrMeta::Path,
                    KEY_VALUE_META => AttrMeta::KeyValue,
                    _ => AttrMeta::Tokens,
                };
                let path = path.map_or(Range::new(r.start, r.start), |p| p.range);
                let args = args.map_or(Range::new(r.end, r.end), |a| a.range);
                let a = Attr { path: BodyRange::building(path), args: BodyRange::building(args), meta, inner: false };
                self.push_node(V::Row(ROW_ATTR), a.encode(), r);
            }
            UNSAFE_META => {
                for i in f.base as usize..self.vals.len() {
                    if self.vals[i].v == V::Row(ROW_ATTR) {
                        self.vals[i].node.flags = (self.vals[i].node.flags & !3) | 3;
                    }
                }
                self.keep(f, |v| matches!(v, V::Row(ROW_ATTR) | V::CfgFalse));
            }
            CFG_META => {
                let p = self.first_pred(f);
                self.done(f);
                if p == CfgAnswer::No {
                    self.push(V::CfgFalse, NONE, r);
                }
            }
            CFG_ATTR_META => {
                let p = self.first_pred(f);
                if p == CfgAnswer::No {
                    self.done(f);
                } else {
                    self.keep(f, |v| matches!(v, V::Row(ROW_ATTR) | V::CfgFalse));
                }
            }
            CFG_ATOM => self.cfg_atom(f, r),
            CFG_COMPOSITE => self.cfg_composite(f, r),
            VISIBILITY => {
                // `pub(crate)`, `pub(self)`, `pub(super)` are one-segment paths; `pub(in p)`
                // any path.
                let path = self.first(f, V::Path).map_or(NONE, |p| p.id);
                let code = match (self.has_tok(f, IN_KW), unopt(path)) {
                    (_, None) => 1,
                    (true, Some(_)) => 5,
                    (false, Some(p)) => match self.single_segment(PathIdx(p)).map(|s| s.kind) {
                        Some(SegKind::Crate) => 2,
                        Some(SegKind::SelfMod) => 3,
                        Some(SegKind::Super) => 4,
                        _ => 5,
                    },
                };
                self.done(f);
                let mut node = Node::of(0);
                node.flags = code;
                node.a = path;
                self.push_node(V::Vis, node, r);
            }
            // Fields, variants, associated items.
            RECORD_FIELD_LIST | TUPLE_FIELD_LIST => {
                let l = self.put_rows(f, ROW_FIELD);
                self.done(f);
                let shape = if f.kind == RECORD_FIELD_LIST { Shape::Record } else { Shape::Tuple };
                self.push_list(L::Fields(shape), l, r);
            }
            RECORD_FIELD | TUPLE_FIELD => {
                let attrs = self.put_rows(f, ROW_ATTR);
                let vis = self.vis_of(f);
                let name = self.first(f, V::Name).map(|v| NameIdx(v.id));
                let ty = self.first(f, V::Type);
                // The default is the node after `=`, skipped; a doc comment is skipped too and is
                // no anonymous constant.
                let default = self.kid(f, 0, |v| matches!(v, V::Skipped(k) if k != DOC_COMMENT));
                self.done(f);
                let ty = ty.map_or_else(|| self.put_type(TypeRef::Error, r), |v| TypeId(v.id));
                let default = default.map(|d| self.put_const(d.range));
                let field = Field { name, vis, ty, attrs, default };
                self.push_node(V::Row(ROW_FIELD), field.encode(), r);
            }
            VARIANT_LIST => {
                let l = self.put_rows(f, ROW_VARIANT);
                self.done(f);
                self.push_list(L::Variants, l, r);
            }
            VARIANT => {
                let attrs = self.put_rows(f, ROW_ATTR);
                let name = self.first(f, V::Name);
                let fields = self
                    .kid(f, 0, |v| matches!(v, V::List(L::Fields(_))))
                    .map(|v| (List { start: v.id, len: v.node.a }, v.v));
                let disc = self.kid(f, 0, |v| matches!(v, V::Skipped(k) if k != DOC_COMMENT));
                self.done(f);
                let (fields, shape) = match fields {
                    Some((l, V::List(L::Fields(shape)))) => (l, shape),
                    _ => (List { start: 0, len: 0 }, Shape::Unit),
                };
                let discriminant = disc.map(|d| self.put_const(d.range));
                if let Some(name) = name {
                    let v = Variant { name: NameIdx(name.id), shape, fields, discriminant, attrs };
                    self.push_node(V::Row(ROW_VARIANT), v.encode(), r);
                }
            }
            ASSOC_ITEM_LIST => {
                let mut nodes: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    if let V::Skipped(kind) = v.v {
                        nodes.push((v.node, v.range));
                        let last = nodes.len() - 1;
                        nodes[last].0.tag = kind as u16;
                    }
                }
                self.done(f);
                let mut rows: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
                for (n, range) in nodes.iter() {
                    let name = (n.a != NONE).then(|| self.name(Range::new(n.a, n.b)));
                    // Its range only in the side column: an edit of its body leaves the
                    // trait's or impl's rows equal.
                    let item = LocalItem { syntax: n.tag, name, range: BodyRange::building(Range::new(0, 0)), scope: None };
                    rows.push((item.encode(ROW_ASSOC), *range));
                }
                let l = self.put_rows_of(&rows);
                self.push_list(L::Items, l, r);
            }
            // Items: the root's, or one skipped.
            FN | STRUCT | ENUM | UNION | TRAIT | IMPL | TYPE_ALIAS | CONST | STATIC | MODULE | USE | MACRO_RULES
            | MACRO_DEF | EXTERN_BLOCK | EXTERN_CRATE
                if self.is_root_frame() =>
            {
                match self.mode {
                    Mode::Signature => self.signature_root(f),
                    _ => self.body_root(f, r),
                }
            }
            // Statements and blocks.
            STMT_LIST => self.stmt_list(f, r),
            BLOCK_EXPR => self.block(f, r),
            TRY_BLOCK_MODIFIER => {
                self.done(f);
                self.top_mut().flags |= F_TRY;
            }
            LABEL => {
                let l = self.first(f, V::Lifetime);
                self.done(f);
                if let Some(l) = l {
                    let hash = self.local_key(NameIdx(l.id));
                    match self.parent_kind() {
                        Some(LOOP_EXPR | WHILE_EXPR) => {
                            if let Some(e) = self.entries.last_mut() {
                                e.label = hash;
                            }
                        }
                        Some(FOR_EXPR) => self.top_mut().aux = l.id,
                        Some(BLOCK_EXPR) => {
                            self.push_entry(Jk::Block, hash);
                            self.top_mut().flags |= F_LOOP;
                        }
                        _ => {}
                    }
                }
            }
            LET_STMT => {
                let pat = self.first(f, V::Pat).map(|v| PatId(v.id));
                let ty = self.first(f, V::Type).map(|v| TypeId(v.id));
                let init = self.first(f, V::Expr).map(|v| ExprId(v.id));
                let else_ = self.first(f, V::Else).map(|v| ExprId(v.id));
                // `super let` (parser grammar/expressions.rs `let_stmt` eats the `super` as a
                // token of this node): kept as a bit of the row (super_let.rs).
                let is_super = self.has_tok(f, SUPER_KW);
                self.done(f);
                let pat = pat.unwrap_or_else(|| self.put_pat(Pat::Missing, r));
                let s = self.put_stmt(Stmt::Let { pat, ty, init, else_ });
                if is_super && let Some(row) = self.out.stmts.get_mut(s.0 as usize) {
                    row.flags |= crate::super_let::LET_SUPER;
                }
                self.activate(f.pending);
                self.push(V::Stmt, s.0, r);
            }
            LET_ELSE => {
                let e = self.first(f, V::Expr);
                self.done(f);
                if let Some(e) = e {
                    self.push(V::Else, e.id, r);
                }
            }
            EXPR_STMT
                if self.vals[f.base as usize..]
                    .iter()
                    .any(|v| v.v == V::Stmt || matches!(v.v, V::Skipped(k) if is_item(k)))
                    || self.count(f, |v| v == V::Expr) > 1 =>
            {
                // A macro call's statements, lowered in place: they stay the block's, and a
                // last expression is a statement of its own when a `;` follows the call.
                // (More than one expression: an expansion of block-like statements, each left
                // a `V::Expr` by the arm below; `stmt_list` decides which one is the tail.)
                let semi = self.has_tok(f, SEMICOLON);
                let last = self.vals[f.base as usize..].iter().rposition(|v| v.v == V::Expr);
                if semi && let Some(at) = last {
                    let at = f.base as usize + at;
                    let v = self.vals[at];
                    let s = self.put_stmt(Stmt::Expr { expr: ExprId(v.id), semi: true });
                    self.vals[at] = Val { v: V::Stmt, id: s.0, ..v };
                }
            }
            EXPR_STMT => {
                let e = self.first(f, V::Expr);
                let semi = self.has_tok(f, SEMICOLON);
                self.done(f);
                if let Some(e) = e {
                    if semi {
                        let s = self.put_stmt(Stmt::Expr { expr: ExprId(e.id), semi: true });
                        self.push(V::Stmt, s.0, r);
                    } else {
                        // A block-like expression with no `;` (the parser's EXPR_STMT for one
                        // that is not followed by `}`) is not yet a statement: whether it is the
                        // block's final operand is known only after cfg stripping, when a later
                        // `#[cfg(..)]`-false statement has left nothing (the Reference,
                        // "Conditional compilation": "If any predicate is false, the form is
                        // removed from the source code"). `stmt_list` makes the last expression
                        // with nothing after it the tail and every other one a
                        // `Stmt::Expr { semi: false }`.
                        self.vals.push(e);
                    }
                }
            }
            MATCH_GUARD => {
                let e = self.first(f, V::Expr);
                self.done(f);
                if let Some(e) = e {
                    self.push(V::Guard, e.id, r);
                }
            }
            // `asm!`'s parts, each packed into one value its `ASM_EXPR` reads (`asm.rs`).
            ASM_OPERAND_NAMED | ASM_CLOBBER_ABI | ASM_OPTIONS => self.asm_part(f, r),
            MACRO_CALL => self.macro_call(f, r),
            _ => self.expr_or_pat(f, r),
        }
    }

    fn expr_or_pat(&mut self, f: &Frame, r: Range) {
        let e = match f.kind {
            PATH_EXPR => {
                let p = self.first(f, V::Path);
                self.done(f);
                match p {
                    Some(p) => self.path_expr(PathIdx(p.id)),
                    None => Expr::Missing,
                }
            }
            PAREN_EXPR => {
                let e = self.first(f, V::Expr);
                self.done(f);
                e.map_or(Expr::Missing, |e| Expr::Paren(ExprId(e.id)))
            }
            TUPLE_EXPR => {
                let l = self.put_ids(f, V::Expr);
                self.done(f);
                Expr::Tuple { elems: l }
            }
            ARRAY_EXPR => {
                if self.has_tok(f, SEMICOLON) {
                    let elem = self.kid(f, 0, |v| v == V::Expr);
                    let len = self.kid(f, 1, |v| v == V::Expr);
                    self.done(f);
                    match (elem, len) {
                        (Some(a), Some(b)) => Expr::Repeat { elem: ExprId(a.id), len: ExprId(b.id) },
                        _ => Expr::Missing,
                    }
                } else {
                    let l = self.put_ids(f, V::Expr);
                    self.done(f);
                    Expr::Array { elems: l }
                }
            }
            ARG_LIST => {
                let l = self.put_ids(f, V::Expr);
                self.done(f);
                self.push_list(L::Exprs, l, r);
                return;
            }
            CALL_EXPR => {
                let callee = self.first(f, V::Expr);
                let args = self.list_of(f, L::Exprs).map_or(List { start: 0, len: 0 }, |l| l.0);
                self.done(f);
                callee.map_or(Expr::Missing, |c| Expr::Call { callee: ExprId(c.id), args })
            }
            METHOD_CALL_EXPR => {
                let receiver = self.first(f, V::Expr);
                let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
                let gargs = self.list_of(f, L::GArgs);
                let args = self.list_of(f, L::Exprs).map_or(List { start: 0, len: 0 }, |l| l.0);
                self.done(f);
                match (receiver, name) {
                    (Some(recv), Some(name)) => {
                        let m = MethodCall {
                            receiver: ExprId(recv.id),
                            name: NameIdx(name.id),
                            generic_args: gargs.map_or(List { start: 0, len: 0 }, |g| g.0),
                            has_args: gargs.is_some(),
                            args,
                        };
                        let row = self.put_row(m.encode(), r);
                        Expr::MethodCall { row, scope: self.scope() }
                    }
                    _ => Expr::Missing,
                }
            }
            FIELD_EXPR => {
                let base = self.first(f, V::Expr);
                let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
                self.done(f);
                match (base, name) {
                    (Some(b), Some(n)) if n.v == V::NameRef(INT_NUMBER) => {
                        let text = self.out.name_ranges[n.id as usize].of(self.text());
                        match text.parse::<u32>() {
                            Ok(index) => Expr::TupleField { base: ExprId(b.id), index },
                            Err(_) => Expr::Missing,
                        }
                    }
                    (Some(b), Some(n)) => Expr::Field { base: ExprId(b.id), name: NameIdx(n.id) },
                    _ => Expr::Missing,
                }
            }
            INDEX_EXPR => {
                let a = self.kid(f, 0, |v| v == V::Expr);
                let b = self.kid(f, 1, |v| v == V::Expr);
                self.done(f);
                match (a, b) {
                    (Some(a), Some(b)) => Expr::Index { base: ExprId(a.id), index: ExprId(b.id) },
                    _ => Expr::Missing,
                }
            }
            PREFIX_EXPR => {
                let op = self.kid(f, 0, |v| matches!(v, V::Tok(_)));
                let e = self.first(f, V::Expr);
                self.done(f);
                let op = match op.map(|o| o.v) {
                    Some(V::Tok(MINUS)) => UnOp::Neg,
                    Some(V::Tok(BANG)) => UnOp::Not,
                    _ => UnOp::Deref,
                };
                e.map_or(Expr::Missing, |e| Expr::Unary { op, operand: ExprId(e.id) })
            }
            REF_EXPR => {
                let raw = self.has_tok(f, RAW_KW);
                let mutbl = if self.has_tok(f, MUT_KW) || self.has_tok(f, UNIQ_KW) { Mutbl::Mut } else { Mutbl::Not };
                let e = self.first(f, V::Expr);
                self.done(f);
                e.map_or(Expr::Missing, |e| Expr::Ref { mutbl, raw, operand: ExprId(e.id) })
            }
            BIN_EXPR => self.binary(f, r),
            CAST_EXPR => {
                let e = self.first(f, V::Expr);
                let t = self.first(f, V::Type);
                self.done(f);
                match (e, t) {
                    (Some(e), Some(t)) => Expr::Cast { operand: ExprId(e.id), ty: TypeId(t.id) },
                    _ => Expr::Missing,
                }
            }
            RANGE_EXPR => {
                let base = f.base as usize;
                let op = self.vals[base..].iter().position(|v| matches!(v.v, V::Tok(DOT2 | DOT2EQ | DOT3)));
                let inclusive = self.has_tok(f, DOT2EQ) || self.has_tok(f, DOT3);
                let mut lo = None;
                let mut hi = None;
                for (i, v) in self.vals[base..].iter().enumerate() {
                    if v.v == V::Expr {
                        if op.is_some_and(|op| i < op) {
                            lo = Some(ExprId(v.id));
                        } else {
                            hi = Some(ExprId(v.id));
                        }
                    }
                }
                self.done(f);
                let e = self.range_expr(lo, hi, inclusive, r);
                self.push(V::Expr, e.0, r);
                return;
            }
            RECORD_EXPR => {
                let path = self.first(f, V::Path);
                let fields = self.first(f, V::List(L::Inits));
                self.done(f);
                match (path, fields) {
                    (Some(p), Some(l)) => {
                        let base = match l.node.b {
                            1 => StructBase::Expr(ExprId(l.node.c)),
                            2 => StructBase::Defaults,
                            _ => StructBase::None,
                        };
                        Expr::Struct { path: PathIdx(p.id), fields: List { start: l.id, len: l.node.a }, base }
                    }
                    (None, Some(l)) => {
                        let base = match l.node.b {
                            1 => StructBase::Expr(ExprId(l.node.c)),
                            2 => StructBase::Defaults,
                            _ => StructBase::None,
                        };
                        Expr::Record { fields: List { start: l.id, len: l.node.a }, base }
                    }
                    _ => Expr::Missing,
                }
            }
            RECORD_EXPR_FIELD_LIST => {
                let l = self.put_rows(f, ROW_FIELD_INIT);
                let dots = self.vals[f.base as usize..].iter().position(|v| v.v == V::Tok(DOT2));
                let base = dots.and_then(|at| {
                    self.vals[f.base as usize + at..].iter().find(|v| v.v == V::Expr).copied()
                });
                self.done(f);
                let mut node = Node::of(0);
                node.a = l.len;
                (node.b, node.c) = match (dots, base) {
                    (Some(_), Some(b)) => (1, b.id),
                    (Some(_), None) => (2, NONE),
                    _ => (0, NONE),
                };
                self.vals.push(Val { v: V::List(L::Inits), id: l.start, node, range: r });
                return;
            }
            RECORD_EXPR_FIELD => {
                let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
                let e = self.first(f, V::Expr);
                self.done(f);
                let Some(e) = e else { return };
                let (name, shorthand) = match name {
                    Some(n) => (NameIdx(n.id), false),
                    None => (self.name(e.range), true),
                };
                let init = FieldInit { name, expr: ExprId(e.id), shorthand };
                self.push_node(V::Row(ROW_FIELD_INIT), init.encode(), r);
                return;
            }
            IF_EXPR => {
                let c = self.kid(f, 0, |v| v == V::Expr);
                let t = self.kid(f, 1, |v| v == V::Expr);
                let e = self.kid(f, 2, |v| v == V::Expr);
                self.done(f);
                match (c, t) {
                    (Some(c), Some(t)) => {
                        Expr::If { cond: ExprId(c.id), then: ExprId(t.id), else_: e.map(|e| ExprId(e.id)) }
                    }
                    _ => Expr::Missing,
                }
            }
            LET_EXPR => {
                let p = self.first(f, V::Pat);
                let e = self.first(f, V::Expr);
                self.done(f);
                self.activate(f.pending);
                match (p, e) {
                    (Some(p), Some(e)) => Expr::Let { pat: PatId(p.id), init: ExprId(e.id) },
                    _ => Expr::Missing,
                }
            }
            MATCH_EXPR => {
                let s = self.first(f, V::Expr);
                let arms = self.list_of(f, L::Arms).map_or(List { start: 0, len: 0 }, |l| l.0);
                self.done(f);
                s.map_or(Expr::Missing, |s| Expr::Match { scrutinee: ExprId(s.id), arms, source: MatchSource::Normal })
            }
            MATCH_ARM_LIST => {
                let l = self.put_rows(f, ROW_ARM);
                self.done(f);
                self.push_list(L::Arms, l, r);
                return;
            }
            MATCH_ARM => {
                let p = self.first(f, V::Pat);
                let g = self.first(f, V::Guard).map(|v| ExprId(v.id));
                let b = self.first(f, V::Expr);
                self.done(f);
                if let (Some(p), Some(b)) = (p, b) {
                    let arm = Arm { pat: PatId(p.id), guard: g, body: ExprId(b.id) };
                    self.push_node(V::Row(ROW_ARM), arm.encode(), r);
                }
                return;
            }
            LOOP_EXPR => {
                let b = self.first(f, V::Expr);
                self.done(f);
                let body = b.map_or_else(|| self.put_expr(Expr::Missing, r), |b| ExprId(b.id));
                let e = self.put_expr(Expr::Loop { body, source: LoopSource::Loop }, r);
                self.loop_target = e.0;
                self.push(V::Expr, e.0, r);
                return;
            }
            WHILE_EXPR => {
                let cond = self.kid(f, 0, |v| v == V::Expr).map(|v| ExprId(v.id));
                let body = self.kid(f, 1, |v| v == V::Expr).map(|v| ExprId(v.id));
                self.done(f);
                let cond = cond.unwrap_or_else(|| self.put_expr(Expr::Missing, r));
                let body = body.unwrap_or_else(|| self.put_expr(Expr::Missing, r));
                let e = self.while_loop(cond, body, r);
                self.loop_target = e.0;
                self.push(V::Expr, e.0, r);
                return;
            }
            FOR_EXPR => {
                let pat = self.first(f, V::Pat).map(|v| PatId(v.id));
                let head = self.kid(f, 0, |v| v == V::Expr).map(|v| ExprId(v.id));
                let body = self.kid(f, 1, |v| v == V::Expr).map(|v| ExprId(v.id));
                self.done(f);
                let pat = pat.unwrap_or_else(|| self.put_pat(Pat::Missing, r));
                let head = head.unwrap_or_else(|| self.put_expr(Expr::Missing, r));
                let body = body.unwrap_or_else(|| self.put_expr(Expr::Missing, r));
                if f.flags & F_INLINE != 0 {
                    let e = self.put_expr(Expr::InlineFor { pat, iter: head, body }, r);
                    self.push(V::Expr, e.0, r);
                    return;
                }
                let e = self.for_loop(pat, head, body, r);
                if f.flags & F_LOOP != 0 {
                    self.loop_target = e.inner_loop.0;
                }
                self.push(V::Expr, e.outer.0, r);
                return;
            }
            BREAK_EXPR | CONTINUE_EXPR => {
                let label = self.first(f, V::Lifetime).map(|l| NameIdx(l.id));
                let value = self.first(f, V::Expr).map(|v| ExprId(v.id));
                self.done(f);
                let want_loop = f.kind == CONTINUE_EXPR;
                let found = match label {
                    Some(name) => self.find_label(name, want_loop),
                    None => self.find_entry(0, want_loop, usize::MAX),
                };
                let e = if want_loop {
                    self.put_expr(Expr::Continue { target: None }, r)
                } else {
                    self.put_expr(Expr::Break { value, target: None }, r)
                };
                match found {
                    Ok(entry) => self.add_jump(entry, e),
                    Err(kind) => self.diag(kind, r),
                }
                self.push(V::Expr, e.0, r);
                return;
            }
            RETURN_EXPR => {
                let v = self.first(f, V::Expr).map(|v| ExprId(v.id));
                self.done(f);
                Expr::Return { value: v }
            }
            BECOME_EXPR => {
                let v = self.first(f, V::Expr);
                self.done(f);
                v.map_or(Expr::Missing, |v| Expr::Become { value: ExprId(v.id) })
            }
            YIELD_EXPR => {
                let v = self.first(f, V::Expr).map(|v| ExprId(v.id));
                self.done(f);
                if !self.in_coroutine() {
                    self.diag(DiagKind::YieldOutsideCoroutine, r);
                }
                Expr::Yield { value: v }
            }
            TRY_EXPR => {
                let e = self.first(f, V::Expr);
                self.done(f);
                let e = e.map_or_else(|| self.put_expr(Expr::Missing, r), |e| ExprId(e.id));
                let e = self.try_op(e, r);
                self.push(V::Expr, e.0, r);
                return;
            }
            AWAIT_EXPR => {
                let e = self.first(f, V::Expr);
                self.done(f);
                let e = e.map_or_else(|| self.put_expr(Expr::Missing, r), |e| ExprId(e.id));
                let e = self.await_op(e, r);
                self.push(V::Expr, e.0, r);
                return;
            }
            CLOSURE_EXPR => self.closure(f, r),
            // `builtin # format_args("..", args)`, the expander's `format_args!` once its
            // template is expanded: its arguments lowered here as any expression (from the
            // expansion's own words, `$crate` paths and hygiene kept), the template read.
            FORMAT_ARGS_EXPR => {
                let template = self.first(f, V::Expr).map(|v| (ExprId(v.id), v.range));
                let mut args: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
                for v in &self.vals[f.base as usize..] {
                    if v.v == V::Row(ROW_FMT_ARG) {
                        args.push((v.node, v.range));
                    }
                }
                self.done(f);
                let e = match self.builtin_format_args(template, &args, r) {
                    Ok(e) => e,
                    Err(why) => {
                        self.unhandled(f.kind, r, why);
                        self.put_expr(Expr::Missing, r)
                    }
                };
                // Only the template's text is read (fmt.rs `builtin_format_args`): its lowered
                // literal is held by no node, so its slot becomes a hole the checker and the
                // writeback skip, never an expression left untyped.
                if let Some((t, _)) = template {
                    self.out.exprs[t.0 as usize] = Expr::Missing.encode();
                }
                self.push(V::Expr, e.0, r);
                return;
            }
            UNDERSCORE_EXPR => {
                self.done(f);
                Expr::Underscore
            }
            // `builtin # asm(..)`, the form blaze-expand writes for `asm!`: its templates
            // parsed and its operands placed (`asm.rs`, which clears the frame's values);
            // `global_asm!` and `naked_asm!` are refused by name (`Why::Asm`).
            ASM_EXPR => self.asm_expr(f, r),
            OFFSET_OF_EXPR => {
                // `builtin # offset_of(Ty, a.b.0)`: the type, then the field path's names; the
                // lexer makes `0.1` one float, split here at its dot.
                let ty = self.first(f, V::Type).map(|v| TypeId(v.id));
                let start = idx(self.out.ids.len());
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    match v.v {
                        V::NameRef(FLOAT_NUMBER) => {
                            let nr = self.out.name_ranges[v.id as usize];
                            let dot = nr.of(self.text()).find('.');
                            match dot {
                                Some(dot) => {
                                    let dot = idx(dot);
                                    let a = self.name(Range::new(nr.start, nr.start + dot));
                                    let b = self.name(Range::new(nr.start + dot + 1, nr.end));
                                    self.out.ids.extend_from_slice(&[a.0, b.0]);
                                }
                                None => self.out.ids.push(v.id),
                            }
                        }
                        V::NameRef(_) => self.out.ids.push(v.id),
                        _ => {}
                    }
                }
                self.done(f);
                let fields = List { start, len: idx(self.out.ids.len()) - start };
                match ty {
                    Some(ty) if fields.len > 0 => Expr::OffsetOf { ty, fields },
                    _ => Expr::Missing,
                }
            }
            INCLUDE_BYTES_EXPR | YEET_EXPR | UNSAFE_BINDER_CAST_EXPR => {
                self.done(f);
                self.unhandled(f.kind, r, Why::Kind);
                Expr::Missing
            }
            ERROR => {
                self.done(f);
                self.diag(DiagKind::Syntax, r);
                if matches!(self.parent_kind(), Some(STMT_LIST | ARG_LIST | BIN_EXPR)) {
                    Expr::Missing
                } else {
                    return;
                }
            }
            _ => {
                self.pattern(f, r);
                return;
            }
        };
        let id = self.put_expr(e, r);
        self.push(V::Expr, id.0, r);
    }

    fn pattern(&mut self, f: &Frame, r: Range) {
        let p = match f.kind {
            IDENT_PAT => {
                let by_ref = self.has_tok(f, REF_KW);
                let mutable = self.has_tok(f, MUT_KW) || (by_ref && self.has_tok(f, UNIQ_KW));
                let name = self.first(f, V::Name);
                let sub = self.first(f, V::Pat).map(|v| PatId(v.id));
                let ty = self.first(f, V::Type).map(|v| TypeId(v.id));
                self.done(f);
                let Some(name) = name else { return };
                let ann = BindAnn { by_ref: by_ref.then_some(if mutable { Mutbl::Mut } else { Mutbl::Not }), mutable: mutable && !by_ref };
                let name = NameIdx(name.id);
                let p = self.put_pat(Pat::Bind { ann, name, sub, shadows: None, ty }, r);
                // Settled at the end, when every block's items are known: a bare name that
                // is a constant, a unit struct or a unit variant becomes a path pattern.
                let bare = !by_ref && !mutable && sub.is_none() && ty.is_none();
                self.idents.push(Ident { pat: p.0, scope: self.scope().0, bare });
                let hash = self.local_key(name);
                self.push_pending(hash, p);
                self.push(V::Pat, p.0, r);
                return;
            }
            WILDCARD_PAT => {
                self.done(f);
                Pat::Wild
            }
            REST_PAT => {
                self.done(f);
                Pat::Rest
            }
            TUPLE_PAT | TUPLE_STRUCT_PAT => {
                let path = self.first(f, V::Path);
                let (elems, rest) = self.pats_without_rest(f);
                self.done(f);
                match (f.kind, path) {
                    (TUPLE_PAT, _) => Pat::Tuple { elems, rest },
                    (_, Some(p)) => Pat::TupleStruct { path: PathIdx(p.id), elems, rest },
                    _ => Pat::Missing,
                }
            }
            PAREN_PAT => {
                let p = self.first(f, V::Pat);
                self.done(f);
                if let Some(p) = p {
                    self.push(V::Pat, p.id, r);
                }
                return;
            }
            REF_PAT => {
                let mutbl = if self.has_tok(f, MUT_KW) || self.has_tok(f, UNIQ_KW) { Mutbl::Mut } else { Mutbl::Not };
                let p = self.first(f, V::Pat);
                self.done(f);
                p.map_or(Pat::Missing, |p| Pat::Ref { mutbl, inner: PatId(p.id) })
            }
            BOX_PAT => {
                let p = self.first(f, V::Pat);
                self.done(f);
                p.map_or(Pat::Missing, |p| Pat::Box(PatId(p.id)))
            }
            LITERAL_PAT => {
                let neg = self.has_tok(f, MINUS);
                let l = self.first(f, V::Lit);
                self.done(f);
                match l.and_then(|l| Lit::decode(l.node.flags, l.node.a, l.node.b)) {
                    Some(l) if neg => Pat::NegLit(l),
                    Some(l) => Pat::Lit(l),
                    None => Pat::Missing,
                }
            }
            PATH_PAT => {
                let p = self.first(f, V::Path);
                self.done(f);
                p.map_or(Pat::Missing, |p| Pat::Path(PathIdx(p.id)))
            }
            RECORD_PAT => {
                let path = self.first(f, V::Path);
                let fields = self.first(f, V::List(L::PatFields));
                self.done(f);
                match (path, fields) {
                    (Some(p), Some(l)) => Pat::Struct {
                        path: PathIdx(p.id),
                        fields: List { start: l.id, len: l.node.a },
                        rest: l.node.b == 1,
                    },
                    (None, Some(l)) => Pat::Record {
                        fields: List { start: l.id, len: l.node.a },
                        rest: l.node.b == 1,
                    },
                    _ => Pat::Missing,
                }
            }
            RECORD_PAT_FIELD_LIST => {
                let l = self.put_rows(f, ROW_FIELD_PAT);
                let rest = self.vals[f.base as usize..]
                    .iter()
                    .any(|v| v.v == V::Pat && Pat::decode(self.out.pats[v.id as usize]) == Ok(Pat::Rest));
                self.done(f);
                let mut node = Node::of(0);
                node.a = l.len;
                node.b = rest as u32;
                self.vals.push(Val { v: V::List(L::PatFields), id: l.start, node, range: r });
                return;
            }
            RECORD_PAT_FIELD => {
                let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
                let p = self.first(f, V::Pat);
                self.done(f);
                let Some(p) = p else { return };
                if name.is_none() && Pat::decode(self.out.pats[p.id as usize]) == Ok(Pat::Rest) {
                    // `S { a, .. }`: the rest, for the field list.
                    self.push(V::Pat, p.id, r);
                    return;
                }
                let (name, shorthand) = match name {
                    Some(n) => (NameIdx(n.id), false),
                    None => match self.bound_name(PatId(p.id)) {
                        Some(n) => (n, true),
                        None => return,
                    },
                };
                let fp = FieldPat { name, pat: PatId(p.id), shorthand };
                self.push_node(V::Row(ROW_FIELD_PAT), fp.encode(), r);
                return;
            }
            OR_PAT => {
                let l = self.put_ids(f, V::Pat);
                self.done(f);
                Pat::Or { alts: l }
            }
            SLICE_PAT => {
                let start = idx(self.out.ids.len());
                let mut rest = None;
                for i in f.base as usize..self.vals.len() {
                    let v = self.vals[i];
                    if v.v != V::Pat {
                        continue;
                    }
                    let node = self.out.pats[v.id as usize];
                    let is_rest = match Pat::decode(node) {
                        Ok(Pat::Rest) => true,
                        Ok(Pat::Bind { sub: Some(s), .. }) => Pat::decode(self.out.pats[s.0 as usize]) == Ok(Pat::Rest),
                        _ => false,
                    };
                    if is_rest {
                        rest = Some(idx(self.out.ids.len()) - start);
                    }
                    self.out.ids.push(v.id);
                }
                let elems = List { start, len: idx(self.out.ids.len()) - start };
                self.done(f);
                Pat::Slice { elems, rest }
            }
            RANGE_PAT => {
                let op = self.vals[f.base as usize..]
                    .iter()
                    .position(|v| matches!(v.v, V::Tok(DOT2 | DOT2EQ | DOT3)));
                let inclusive = self.has_tok(f, DOT2EQ) || self.has_tok(f, DOT3);
                let mut lo = None;
                let mut hi = None;
                for (i, v) in self.vals[f.base as usize..].iter().enumerate() {
                    if v.v == V::Pat {
                        if op.is_some_and(|op| i < op) {
                            lo = Some(PatId(v.id));
                        } else {
                            hi = Some(PatId(v.id));
                        }
                    }
                }
                self.done(f);
                Pat::Range { lo, hi, inclusive }
            }
            // `!null`, only a pattern type's pattern: its `null` is no binding; the parent
            // `PATTERN_TYPE` reads this token value.
            NOT_NULL => {
                self.done(f);
                self.push(V::Tok(NOT_NULL), 0, r);
                return;
            }
            CONST_BLOCK_PAT | DEREF_PAT | GUARD_PAT | CFG_PRED_PAT => {
                self.done(f);
                self.unhandled(f.kind, r, Why::Kind);
                Pat::Missing
            }
            _ => {
                // A node this lowering reads through: its children's values are its parent's.
                return;
            }
        };
        let id = self.put_pat(p, r);
        self.push(V::Pat, id.0, r);
    }

    // -----------------------------------------------------------------------------------------
    // Pieces

    fn push_list(&mut self, l: L, list: List, r: Range) {
        let mut node = Node::of(0);
        node.a = list.len;
        self.vals.push(Val { v: V::List(l), id: list.start, node, range: r });
    }

    /// Keeps the children of `f` that `pred` takes, as values of its parent, in order.
    fn keep(&mut self, f: &Frame, pred: impl Fn(V) -> bool) {
        let base = f.base as usize;
        let mut to = base;
        for i in base..self.vals.len() {
            if pred(self.vals[i].v) {
                self.vals[to] = self.vals[i];
                to += 1;
            }
        }
        self.vals.truncate(to);
    }

    fn type_of(&mut self, inner: Option<Val>, r: Range, make: impl FnOnce(TypeId) -> TypeRef) {
        let t = match inner {
            Some(inner) => make(TypeId(inner.id)),
            None => TypeRef::Error,
        };
        let t = self.put_type(t, r);
        self.push(V::Type, t.0, r);
    }

    pub(crate) fn put_const(&mut self, range: Range) -> ConstIdx {
        let id = idx(self.out.consts.len());
        self.out.consts.push(range);
        ConstIdx(id)
    }

    fn vis_of(&self, f: &Frame) -> Vis {
        self.first(f, V::Vis).map_or(Vis::Private, |v| Vis::decode(v.node.flags, v.node.a))
    }

    fn variadic(&self, params: List) -> bool {
        let start = params.start as usize;
        self.out.rows[start..start + params.len as usize]
            .iter()
            .any(|n| Param::decode(*n).is_ok_and(|p| p.variadic))
    }

    fn bound_name(&self, mut p: PatId) -> Option<NameIdx> {
        loop {
            match Pat::decode(*self.out.pats.get(p.0 as usize)?).ok()? {
                Pat::Bind { name, .. } => return Some(name),
                Pat::Box(inner) => p = inner,
                _ => return None,
            }
        }
    }

    fn pats_without_rest(&mut self, f: &Frame) -> (List, Option<u32>) {
        let start = idx(self.out.ids.len());
        let mut rest = None;
        for i in f.base as usize..self.vals.len() {
            let v = self.vals[i];
            if v.v != V::Pat {
                continue;
            }
            if Pat::decode(self.out.pats[v.id as usize]) == Ok(Pat::Rest) {
                rest = Some(idx(self.out.ids.len()) - start);
            } else {
                self.out.ids.push(v.id);
            }
        }
        (List { start, len: idx(self.out.ids.len()) - start }, rest)
    }

    fn literal(&mut self, f: &Frame, r: Range) {
        let t = self.kid(f, 0, |v| matches!(v, V::Tok(_)));
        self.done(f);
        let lit = t.and_then(|t| match t.v {
            V::Tok(kind) => lit::lit_of(kind, t.range.of(self.text())),
            _ => None,
        });
        let in_pat = self.parent_kind() == Some(LITERAL_PAT);
        match (lit, in_pat) {
            (Some(l), true) => {
                let (k, a, b) = l.encode();
                let mut node = Node::of(0);
                node.flags = k;
                node.a = a;
                node.b = b;
                self.push_node(V::Lit, node, r);
            }
            (Some(l), false) => {
                let e = self.put_expr(Expr::Lit(l), r);
                self.push(V::Expr, e.0, r);
            }
            (None, _) => {
                self.unhandled(LITERAL, r, Why::Kind);
                if !in_pat {
                    let e = self.put_expr(Expr::Missing, r);
                    self.push(V::Expr, e.0, r);
                }
            }
        }
    }

    fn path_segment(&mut self, f: &Frame, r: Range) {
        if let Some(anchor) = self.first(f, V::Seg) {
            // `<T as Trait>`: the anchor is the segment.
            self.done(f);
            self.vals.push(anchor);
            return;
        }
        let absolute = self.vals.get(f.base as usize).is_some_and(|v| v.v == V::Tok(COLON2));
        let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
        let args = self.list_of(f, L::GArgs);
        let ret = self.first(f, V::Ret).map(|v| TypeId(v.id));
        self.done(f);
        let kind = match name.map(|n| n.v) {
            Some(V::NameRef(SELF_KW)) => SegKind::SelfMod,
            Some(V::NameRef(SUPER_KW)) => SegKind::Super,
            Some(V::NameRef(CRATE_KW)) => SegKind::Crate,
            Some(V::NameRef(SELF_TYPE_KW)) => SegKind::SelfTy,
            _ => SegKind::Ident,
        };
        let seg = Segment {
            kind,
            name: name.map(|n| NameIdx(n.id)),
            args: args.map_or(List { start: 0, len: 0 }, |a| a.0),
            has_args: args.is_some(),
            parenthesized: args.is_some_and(|a| a.1.node.b == 1),
            return_notation: args.is_some_and(|a| a.1.node.b == 2),
            ty: ret,
            trait_: None,
        };
        let mut node = seg.encode();
        node.f = absolute as u32;
        self.push_node(V::Seg, node, r);
    }

    fn path(&mut self, f: &Frame, r: Range) {
        if self.parent_kind() == Some(PATH) {
            // A qualifier: its segments stay for the outer path.
            self.keep(f, |v| v == V::Seg);
            return;
        }
        let absolute = self.first(f, V::Seg).is_some_and(|s| s.node.f == 1);
        let segments = self.vals[f.base as usize..].iter().filter(|v| v.v == V::Seg).count();
        self.out.rows.reserve(segments);
        self.out.row_ranges.reserve(segments);
        let start = idx(self.out.rows.len());
        for i in f.base as usize..self.vals.len() {
            let v = self.vals[i];
            if v.v == V::Seg {
                let mut node = v.node;
                node.f = NONE;
                self.out.rows.push(node);
                self.out.row_ranges.push(v.range);
            }
        }
        let segments = List { start, len: idx(self.out.rows.len()) - start };
        self.done(f);
        // A signature's paths and a lone type's have no body scope.
        let scope = matches!(self.mode, Mode::Body | Mode::AnonConst).then(|| self.scope());
        let p = self.put_path(PathRef { segments, absolute, scope, lang: None }, r);
        self.push(V::Path, p.0, r);
    }

    /// Opens a frame no parser node made, to hold the values of what is fed inside it (a
    /// format argument's expression): its parent sees none of them. Returns its values' base.
    pub(crate) fn open_container(&mut self) -> usize {
        let base = self.vals.len();
        self.frames.push(Frame {
            kind: TOKEN_TREE,
            start: NONE,
            base: idx(base),
            locals: idx(self.locals.len()),
            pending: idx(self.pending.len()),
            saved_barrier: self.barrier,
            // Where the source was: what is fed inside is earlier text, re-read.
            aux: self.last_end,
            flags: 0,
        });
        base
    }

    /// Closes the frame [`Machine::open_container`] opened, dropping its values.
    pub(crate) fn close_container(&mut self, base: usize) {
        let f = self.frames.pop().expect("a container closes what it opened");
        debug_assert_eq!(f.base as usize, base, "a container closes its own frame");
        self.vals.truncate(base);
        self.pending.truncate(f.pending as usize);
        self.last_end = f.aux;
    }

    /// A path in expression position: a local when it is one name in scope.
    pub(crate) fn path_expr(&mut self, p: PathIdx) -> Expr {
        if let Some(seg) = self.single_segment(p)
            && matches!(seg.kind, SegKind::Ident | SegKind::SelfMod)
            && !seg.has_args
            && let Some(name) = seg.name
            && let Some(pat) = self.lookup_name(name)
        {
            return Expr::Local { pat, path: p };
        }
        Expr::Path(p)
    }

    /// The segment of a relative path of one segment.
    fn single_segment(&self, p: PathIdx) -> Option<Segment> {
        let path = PathRef::decode(self.out.paths[p.0 as usize]).ok()?;
        if path.segments.len != 1 || path.absolute {
            return None;
        }
        Segment::decode(self.out.rows[path.segments.start as usize]).ok()
    }

    fn generic_args(&mut self, f: &Frame, r: Range) {
        let mut nodes: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
        for i in f.base as usize..self.vals.len() {
            let v = self.vals[i];
            match v.v {
                V::Row(ROW_ARG) => nodes.push((v.node, v.range)),
                V::Skipped(CONST_ARG) => {
                    let c = self.put_const(v.range);
                    nodes.push((GenericArg::Const(c).encode(), v.range));
                }
                _ => {}
            }
        }
        self.done(f);
        let l = self.put_rows_of(&nodes);
        self.push_list(L::GArgs, l, r);
    }

    fn assoc_type_arg(&mut self, f: &Frame, r: Range) {
        let name = self.kid(f, 0, |v| matches!(v, V::NameRef(_)));
        let args = self.list_of(f, L::GArgs).map_or(List { start: 0, len: 0 }, |l| l.0);
        let ty = self.first(f, V::Type);
        let value = self.first(f, V::Skipped(CONST_ARG));
        let bounds = self.list_of(f, L::Bounds).map(|l| l.0);
        self.done(f);
        let Some(name) = name else { return };
        let name = NameIdx(name.id);
        let arg = match (ty, value, bounds) {
            (Some(t), _, _) => GenericArg::AssocType { name, args, ty: TypeId(t.id) },
            (None, Some(c), _) => GenericArg::AssocConst { name, args, value: self.put_const(c.range) },
            (None, None, Some(bounds)) => GenericArg::AssocBound { name, args, bounds },
            (None, None, None) => return,
        };
        self.push_node(V::Row(ROW_ARG), arg.encode(), r);
    }

    fn type_bound(&mut self, f: &Frame, r: Range) {
        let lifetime = self.first(f, V::Lifetime);
        let ty = self.first(f, V::Type);
        let binder = self.list_of(f, L::GParams).map_or(List { start: 0, len: 0 }, |l| l.0);
        let names = self.list_of(f, L::Names).map(|l| l.0);
        let tilde = self.has_tok(f, TILDE) || self.has_tok(f, L_BRACK);
        let mods = BoundMods {
            maybe: self.has_tok(f, QUESTION),
            negative: self.has_tok(f, BANG),
            konst: self.has_tok(f, CONST_KW) && !tilde,
            maybe_const: self.has_tok(f, CONST_KW) && tilde,
            asyncness: self.has_tok(f, ASYNC_KW),
        };
        self.done(f);
        let b = match (ty, lifetime, names) {
            (Some(t), _, _) => Bound::Trait { ty: TypeId(t.id), binder, mods },
            (None, Some(l), _) => Bound::Lifetime(NameIdx(l.id)),
            (None, None, Some(names)) => Bound::Use(names),
            (None, None, None) => return,
        };
        self.push_node(V::Row(ROW_BOUND), b.encode(), r);
    }

    fn param_list(&mut self, f: &Frame, r: Range) {
        let l = self.put_rows(f, ROW_PARAM);
        self.done(f);
        let parent = self.frames.last().copied();
        match parent.map(|p| p.kind) {
            Some(CLOSURE_EXPR) => {
                self.activate(f.pending);
                // A plain closure is no coroutine: `.await` and `yield` in it are refused, as
                // rustc does, though it sits in one.
                let kind = parent.map_or(0, |p| p.flags & (F_ASYNC | F_GEN));
                self.coros.push(if kind != 0 { kind } else { NOT_CORO });
                self.top_mut().flags |= F_CORO;
            }
            // The root fn's own parameters (the root's frame is still open above this list).
            Some(FN) if self.mode == Mode::Body && self.frames.len() == self.root_depth + 1 => {
                self.activate(f.pending)
            }
            // A fn pointer's names, a signature's patterns: bound nowhere.
            _ => self.pending.truncate(f.pending as usize),
        }
        self.push_list(L::Params, l, r);
    }

    fn self_param(&mut self, f: &Frame, r: Range) {
        let amp = self.has_tok(f, AMP);
        let mutable = self.has_tok(f, MUT_KW) || (amp && self.has_tok(f, UNIQ_KW));
        let lifetime = self.first(f, V::Lifetime).map(|v| NameIdx(v.id));
        let name = self.first(f, V::Name);
        let ty = self.first(f, V::Type).map(|v| TypeId(v.id));
        self.done(f);
        let self_kind = match (amp, ty) {
            (true, _) => SelfKind::Ref(if mutable { Mutbl::Mut } else { Mutbl::Not }),
            (false, Some(_)) => SelfKind::Typed,
            (false, None) => SelfKind::Value,
        };
        let name = match name {
            Some(n) => NameIdx(n.id),
            None => self.synth_name("self", r),
        };
        let ann = BindAnn { by_ref: None, mutable: mutable && !amp };
        let pat = self.put_pat(Pat::Bind { ann, name, sub: None, shadows: None, ty: None }, r);
        let hash = self.local_key(name);
        self.push_pending(hash, pat);
        let p = Param { pat: Some(pat), ty, self_kind, lifetime, variadic: false, attrs: List { start: 0, len: 0 } };
        self.push_node(V::Row(ROW_PARAM), p.encode(), r);
    }

    /// The open item of a block ([`F_ATTRS`]) has no false `cfg` among its outer attributes:
    /// the rest of it is skipped wholesale from here, as [`Self::should_skip`] skips it, its
    /// attributes' values dropped (its own lowering reads them). Its exit leaves
    /// `V::Skipped(kind)` over its whole range, attributes included, as before.
    fn attrs_read(&mut self) {
        let top = self.top_mut();
        top.flags = (top.flags & !F_ATTRS) | F_SKIP;
        let base = top.base as usize;
        self.vals.truncate(base);
        self.skip = 1;
        self.skip_name = NO_RANGE;
        self.skip_arm = false;
    }

    fn attr(&mut self, f: &Frame, r: Range) {
        if self.count(f, |v| v == V::CfgFalse) > 0 {
            self.done(f);
            self.push(V::CfgFalse, NONE, r);
            return;
        }
        let inner = self.has_tok(f, BANG);
        for i in f.base as usize..self.vals.len() {
            if inner && self.vals[i].v == V::Row(ROW_ATTR) {
                self.vals[i].node.flags |= 4;
            }
        }
        self.keep(f, |v| v == V::Row(ROW_ATTR));
    }

    fn first_pred(&self, f: &Frame) -> CfgAnswer {
        match self.kid(f, 0, |v| matches!(v, V::Pred(_))).map(|v| v.v) {
            Some(V::Pred(a)) => a,
            _ => CfgAnswer::Yes,
        }
    }

    fn cfg_atom(&mut self, f: &Frame, r: Range) {
        let first = self.kid(f, 0, |v| matches!(v, V::Tok(_)));
        let value = self.kid(f, 0, |v| matches!(v, V::Tok(STRING)));
        self.done(f);
        let answer = match first.map(|t| t.v) {
            Some(V::Tok(TRUE_KW)) => CfgAnswer::Yes,
            Some(V::Tok(FALSE_KW)) => CfgAnswer::No,
            Some(V::Tok(_)) => {
                let name_range = first.map_or(r, |t| t.range);
                // Copied out of the text, which an expansion lowered here may have grown.
                let mut name = AString::new_in(self.alloc);
                name.push_str(name_range.of(self.text()));
                let value_owned = value.map(|v| {
                    let t = v.range.of(self.text());
                    let mut s = AString::new_in(self.alloc);
                    s.push_str(t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(t));
                    s
                });
                let name = name.as_str();
                let value_text = value_owned.as_deref();
                let Some(cfg) = self.cfg.as_deref_mut() else {
                    unreachable!("the cfg set is lent only while a call expands, and no step runs then")
                };
                let answer = cfg.atom(name, value_text);
                let read = CfgRead {
                    name: Name::of(name).0,
                    value: value_text.map_or(0, |v| Name::of(v).0),
                    answer: answer == CfgAnswer::Yes,
                };
                match answer {
                    CfgAnswer::Need => self.needs.push(r),
                    _ => self.out.cfg.push(read.encode()),
                }
                answer
            }
            _ => CfgAnswer::Yes,
        };
        self.push(V::Pred(answer), NONE, r);
    }

    fn cfg_composite(&mut self, f: &Frame, r: Range) {
        let op: Option<&'static str> = self.kid(f, 0, |v| matches!(v, V::Tok(IDENT))).map(|t| {
            ["all", "any", "not"].into_iter().find(|it| *it == t.range.of(self.text())).unwrap_or("")
        });
        let mut all = CfgAnswer::Yes;
        let mut any = CfgAnswer::No;
        let mut count = 0;
        let mut only = CfgAnswer::Yes;
        for v in &self.vals[f.base as usize..] {
            if let V::Pred(a) = v.v {
                count += 1;
                only = a;
                all = match (all, a) {
                    (CfgAnswer::No, _) | (_, CfgAnswer::No) => CfgAnswer::No,
                    (CfgAnswer::Need, _) | (_, CfgAnswer::Need) => CfgAnswer::Need,
                    _ => CfgAnswer::Yes,
                };
                any = match (any, a) {
                    (CfgAnswer::Yes, _) | (_, CfgAnswer::Yes) => CfgAnswer::Yes,
                    (CfgAnswer::Need, _) | (_, CfgAnswer::Need) => CfgAnswer::Need,
                    _ => CfgAnswer::No,
                };
            }
        }
        self.done(f);
        let answer = match op {
            Some("all") => all,
            Some("any") => any,
            Some("not") if count == 1 => match only {
                CfgAnswer::Yes => CfgAnswer::No,
                CfgAnswer::No => CfgAnswer::Yes,
                CfgAnswer::Need => CfgAnswer::Need,
            },
            _ => {
                // rustc refuses an unknown predicate; so does the lowering, by a diagnostic.
                self.diag(DiagKind::Syntax, r);
                CfgAnswer::No
            }
        };
        self.push(V::Pred(answer), NONE, r);
    }

    fn binary(&mut self, f: &Frame, _r: Range) -> Expr {
        let lhs = self.kid(f, 0, |v| v == V::Expr);
        let rhs = self.kid(f, 1, |v| v == V::Expr);
        let op = self.vals[f.base as usize..].iter().find_map(|v| match v.v {
            V::Tok(k) if k == EQ || bin_op(k).is_some() => Some(k),
            _ => None,
        });
        self.done(f);
        let (Some(lhs), Some(rhs), Some(op)) = (lhs, rhs, op) else { return Expr::Missing };
        let (lhs, rhs) = (ExprId(lhs.id), ExprId(rhs.id));
        if op == EQ {
            // An assignee that is a tuple, a slice, a tuple struct or a struct (in parentheses
            // or not) destructures; `..` alone and `_` alone stay one assignment.
            let mut place = lhs;
            for _ in 0..=self.out.exprs.len() {
                match Expr::decode(self.out.exprs[place.0 as usize]) {
                    Ok(Expr::Paren(inner)) => place = inner,
                    Ok(Expr::Tuple { .. } | Expr::Array { .. } | Expr::Call { .. }) => return self.destructuring_assign(lhs, rhs),
                    Ok(Expr::Struct { path, fields, .. }) => {
                        let range_full = PathRef::decode(self.out.paths[path.0 as usize])
                            .is_ok_and(|p| p.lang == Some(LangItem::RangeFull));
                        if range_full && fields.len == 0 {
                            break;
                        }
                        return self.destructuring_assign(lhs, rhs);
                    }
                    _ => break,
                }
            }
            return Expr::Assign { lhs, rhs };
        }
        match bin_op(op) {
            Some((op, true)) => Expr::AssignOp { op, lhs, rhs },
            Some((op, false)) => Expr::Binary { op, lhs, rhs },
            None => Expr::Missing,
        }
    }

    fn closure(&mut self, f: &Frame, r: Range) -> Expr {
        let params = self.list_of(f, L::Params).map_or(List { start: 0, len: 0 }, |l| l.0);
        let ret = self.first(f, V::Ret).map(|v| TypeId(v.id));
        let body = self.first(f, V::Expr);
        self.done(f);
        let body = body.map_or_else(|| self.put_expr(Expr::Missing, r), |b| ExprId(b.id));
        let kind = match (f.flags & F_ASYNC != 0, f.flags & F_GEN != 0) {
            (true, true) => CoroutineKind::AsyncGen,
            (true, false) => CoroutineKind::Async,
            (false, true) => CoroutineKind::Gen,
            (false, false) => CoroutineKind::None,
        };
        let c = Closure {
            params,
            ret,
            body,
            kind,
            source: CoroutineSource::Closure,
            capture_by_value: f.flags & F_MOVE != 0,
            konst: f.flags & F_CONST != 0,
            immovable: f.flags & F_STATIC != 0,
        };
        let row = self.put_row(c.encode(), r);
        Expr::Closure { row }
    }

    fn stmt_list(&mut self, f: &Frame, r: Range) {
        // The items declared in the block: rows of the scope, in order.
        let mut items: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
        let scope = self.scope();
        for i in f.base as usize..self.vals.len() {
            let v = self.vals[i];
            if let V::Skipped(kind) = v.v
                && is_item(kind)
            {
                let mut n = v.node;
                n.tag = kind as u16;
                items.push((n, v.range));
            }
        }
        let mut rows: AVec<'_, (Node, Range)> = Vec::new_in(self.vals.allocator().clone());
        for (n, range) in items.iter() {
            let name = (n.a != NONE).then(|| self.name(Range::new(n.a, n.b)));
            let item = LocalItem { syntax: n.tag, name, range: BodyRange::building(*range), scope: Some(scope) };
            rows.push((item.encode(ROW_LOCAL_ITEM), *range));
        }
        let item_rows = self.put_rows_of(&rows);
        // Which text wrote the block (crate::scope_origin): `r` starts at its `{`.
        let origin = self.origin_of_block(r.start);
        let s = &mut self.out.scopes[scope.0 as usize];
        s.b = item_rows.start;
        s.c = item_rows.len;
        origin.write(s);
        // The statements, and the tail: the last expression with nothing after it.
        let last_stmt = self.vals[f.base as usize..]
            .iter()
            .rposition(|v| matches!(v.v, V::Stmt | V::Expr) || matches!(v.v, V::Skipped(k) if is_item(k)));
        let mut tail = None;
        let mut ids: AVec<'_, u32> = Vec::new_in(self.vals.allocator().clone());
        let mut item_at = 0u32;
        for (i, v) in self.vals[f.base as usize..].iter().enumerate() {
            match v.v {
                V::Stmt => ids.push(v.id),
                V::Expr if Some(i) == last_stmt => tail = Some(ExprId(v.id)),
                V::Expr => {
                    let id = idx(self.out.stmts.len());
                    self.out.stmts.push(Stmt::Expr { expr: ExprId(v.id), semi: false }.encode());
                    ids.push(id);
                }
                V::Skipped(k) if is_item(k) => {
                    let id = idx(self.out.stmts.len());
                    self.out.stmts.push(Stmt::Item { row: RowIdx(item_rows.start + item_at) }.encode());
                    item_at += 1;
                    ids.push(id);
                }
                _ => {}
            }
        }
        self.done(f);
        let stmts = self.put_ids_of(&ids);
        let mut node = Node::of(0);
        node.a = stmts.start;
        node.b = stmts.len;
        node.c = opt(tail.map(|t| t.0));
        node.d = scope.0;
        self.push_node(V::StmtList, node, r);
    }

    fn block(&mut self, f: &Frame, r: Range) {
        let list = self.first(f, V::StmtList);
        self.done(f);
        let Some(list) = list else {
            let e = self.put_expr(Expr::Missing, r);
            self.push(V::Expr, e.0, r);
            return;
        };
        let stmts = List { start: list.node.a, len: list.node.b };
        let tail = unopt(list.node.c).map(ExprId);
        let scope = ScopeId(list.node.d);
        if f.flags & (F_ASYNC | F_GEN) != 0 {
            let body = self.put_expr(Expr::Block { stmts, tail, scope, kind: BlockKind::Plain }, r);
            let kind = match (f.flags & F_ASYNC != 0, f.flags & F_GEN != 0) {
                (true, true) => CoroutineKind::AsyncGen,
                (true, false) => CoroutineKind::Async,
                _ => CoroutineKind::Gen,
            };
            let c = Closure {
                params: List { start: 0, len: 0 },
                ret: None,
                body,
                kind,
                source: CoroutineSource::Block,
                capture_by_value: f.flags & F_MOVE != 0,
                konst: false,
                immovable: false,
            };
            let row = self.put_row(c.encode(), r);
            let e = self.put_expr(Expr::Closure { row }, r);
            self.push(V::Expr, e.0, r);
            return;
        }
        let kind = if f.flags & F_UNSAFE != 0 {
            BlockKind::Unsafe
        } else if f.flags & F_CONST != 0 {
            BlockKind::Const
        } else if f.flags & F_TRY != 0 {
            BlockKind::Try
        } else {
            BlockKind::Plain
        };
        let tail = if kind == BlockKind::Try { Some(self.try_block_tail(tail, r)) } else { tail };
        let e = self.put_expr(Expr::Block { stmts, tail, scope, kind }, r);
        if f.flags & F_LOOP != 0 {
            self.loop_target = e.0;
        }
        self.push(V::Expr, e.0, r);
    }

    fn macro_call(&mut self, f: &Frame, r: Range) {
        let path = self.first(f, V::Path);
        let input = self.first(f, V::Skipped(TOKEN_TREE)).map_or(Range::new(r.end, r.end), |v| v.range);
        self.done(f);
        let Some(path) = path else { return };
        let path = PathIdx(path.id);
        let place = match self.parent_kind() {
            Some(MACRO_PAT) => MacroPlace::Pat,
            Some(MACRO_TYPE) => MacroPlace::Type,
            Some(STMT_LIST) => MacroPlace::Stmt,
            Some(MACRO_EXPR) => MacroPlace::Expr,
            _ => MacroPlace::Item,
        };
        let mut why = Why::Kind;
        // `format_args!` and its kin stay one node, their template parsed here (`fmt`): by
        // name, written plainly or through `$crate`, `core` or `std` as the library's own
        // macros write it. While expanding, the expander's `format_args!` gives
        // `builtin # format_args(..)` (its template expanded first), whose arguments are then
        // lowered from the expansion's words like any expression: an argument written in an
        // expansion (`$crate::format_args!(..)`) is never read again from text.
        if !self.expanding
            && matches!(place, MacroPlace::Expr | MacroPlace::Stmt)
            && let Some(name) = self.format_args_name(path)
        {
            match self.format_args(name == "format_args_nl", input, r) {
                Ok(e) => {
                    self.push(V::Expr, e.0, r);
                    return;
                }
                Err(w) => why = w,
            }
        }
        // `asm!` expands (blaze-expand's `builtin # asm(..)`, lowered by `asm.rs`);
        // `global_asm!` and `naked_asm!` have no lowering yet: refused by name, never expanded.
        if self.library_macro(path, &["global_asm", "naked_asm"]).is_some() {
            why = Why::Asm;
        }
        // A statement of a block, `m!(..);` among them, expands to statements (Rust's rule
        // for a call in statement position); every other place to its own fragment.
        // A BRACED call that is its block's tail (`fn f() -> T { m! { .. } }`, the parser's
        // `MACRO_EXPR` straight under the statement list, std's `current_os_id` on
        // `cfg_select!`) is a statement too: the Reference, "Macros": "When used as an item or
        // a statement, the MacroInvocationSemi form is used where a semicolon is required at
        // the end when not using curly braces", so a braced call needs no `;` to be one; its
        // expansion is statements whose last expression, with nothing after it, is the block's
        // tail (`stmt_list`). A tail call in parentheses or brackets stays an expression.
        let braced_tail = place == MacroPlace::Expr
            && self.frames.len() >= 2
            && matches!(self.frames[self.frames.len() - 2].kind, STMT_LIST | MACRO_STMTS)
            && input.of(self.text()).starts_with('{');
        let in_stmt = place == MacroPlace::Stmt
            || braced_tail
            || place == MacroPlace::Expr
                && self.frames.len() >= 2
                && self.frames[self.frames.len() - 2].kind == EXPR_STMT;
        let fragment = match place {
            MacroPlace::Pat => blaze_expand::Fragment::Pat,
            MacroPlace::Type => blaze_expand::Fragment::Type,
            _ if in_stmt => blaze_expand::Fragment::Stmts,
            _ => blaze_expand::Fragment::Expr,
        };
        let path_range = self.path_range(path, r);
        let request = Request { range: r, path, path_range, input, place, fragment };
        if !self.expanding || place == MacroPlace::Item || why != Why::Kind {
            self.request = None;
            self.unexpanded(request, why);
            return;
        }
        self.request = Some(request);
    }

    /// The range of a path's text: from the call's start to its last segment's name.
    fn path_range(&self, p: PathIdx, call: Range) -> Range {
        let end = self
            .segment_name(p, usize::MAX)
            .map_or(call.start, |name| self.out.name_ranges[name.0 as usize].end);
        Range::new(call.start, end.max(call.start))
    }

    /// The name of segment `i` of a path (`usize::MAX` for the last).
    fn segment_name(&self, p: PathIdx, i: usize) -> Option<NameIdx> {
        let path = PathRef::decode(self.out.paths[p.0 as usize]).ok()?;
        let len = path.segments.len as usize;
        let i = if i == usize::MAX { len.checked_sub(1)? } else { i };
        if i >= len {
            return None;
        }
        Segment::decode(self.out.rows[path.segments.start as usize + i]).ok()?.name
    }

    /// `format_args`, `format_args_nl` or `const_format_args` when the path names one of
    /// them.
    fn format_args_name(&self, p: PathIdx) -> Option<&'static str> {
        self.library_macro(p, &["format_args", "format_args_nl", "const_format_args"])
    }

    /// The one of `names` the path names as the library's macro: one plain segment, or a path
    /// from `$crate`, `core` or `std` (`core::arch::asm`).
    fn library_macro(&self, p: PathIdx, names: &[&'static str]) -> Option<&'static str> {
        let path = PathRef::decode(self.out.paths[p.0 as usize]).ok()?;
        let text = |name: NameIdx| self.out.name_ranges[name.0 as usize].of(self.text());
        match path.segments.len {
            1 => {}
            // An expansion's body word keeps `$crate` as written, its crate in its mark
            // (blaze-expand's `body_words`); a macro-written item's text writes another crate's
            // `$crate` as `crate`, its crate in that word's mark (blaze-scope crate_roots
            // `root_mark`, read by the one rule `root_by_mark`).
            2 | 3 => {
                let first = self.segment_name(p, 0)?;
                let word = text(first);
                let root_of_a_crate = word == "crate"
                    && matches!(
                        blaze_scope::crate_roots::root_by_mark(word, self.mark_at(self.out.name_ranges[first.0 as usize].start)),
                        blaze_scope::crate_roots::Root::Crate(_)
                    );
                if !(matches!(word, "$crate" | "core" | "std") || root_of_a_crate) {
                    return None;
                }
            }
            _ => return None,
        }
        let name = text(self.segment_name(p, usize::MAX)?);
        names.iter().copied().find(|it| *it == name)
    }

    fn signature_root(&mut self, f: &Frame) {
        let attrs = self.put_rows(f, ROW_ATTR);
        let vis = self.vis_of(f);
        let name = self.first(f, V::Name).map_or(NONE, |v| v.id);
        let generics = self.list_of(f, L::GParams).map(|l| l.0);
        let where_ = self.list_of(f, L::Where).map(|l| l.0);
        let params = self.list_of(f, L::Params).map(|l| l.0);
        let ret = self.first(f, V::Ret).map(|v| v.id);
        let abi = self.first(f, V::Abi);
        let fields = self
            .kid(f, 0, |v| matches!(v, V::List(L::Fields(_) | L::Variants)))
            .map(|v| (List { start: v.id, len: v.node.a }, v.v));
        let bounds = self.list_of(f, L::Bounds).map(|l| l.0);
        let items = self.list_of(f, L::Items).map(|l| l.0);
        let t0 = self.kid(f, 0, |v| v == V::Type).map(|v| v.id);
        let t1 = self.kid(f, 1, |v| v == V::Type).map(|v| v.id);
        // A fn's body is a lazy node of the outline, or a block skipped; a const's or
        // static's value follows its `=`.
        let block = self.first(f, V::Skipped(BLOCK_EXPR));
        let body = block.is_some() || f.flags & F_EQ != 0;
        if f.kind == FN && let Some(block) = block {
            self.out.head[head::FN_TEXT_PREFIX] = block.range.start;
        }
        let mut flags = 0;
        for (tok, flag) in [
            (CONST_KW, sig::CONST),
            (ASYNC_KW, sig::ASYNC),
            (UNSAFE_KW, sig::UNSAFE),
            (DEFAULT_KW, sig::DEFAULT),
            (SAFE_KW, sig::SAFE),
            (GEN_KW, sig::GEN),
            (AUTO_KW, sig::AUTO),
            (BANG, sig::NEGATIVE),
            (MUT_KW, sig::MUT),
            (UNDERSCORE, sig::UNDERSCORE),
        ] {
            if self.has_tok(f, tok) {
                flags |= flag;
            }
        }
        if abi.is_some() {
            flags |= sig::EXTERN;
        }
        if f.kind == TRAIT && self.has_tok(f, EQ) {
            flags |= sig::TRAIT_ALIAS;
        }
        let has_for = self.has_tok(f, FOR_KW);
        self.done(f);
        if matches!(f.kind, FN | CONST | STATIC) && body {
            flags |= sig::HAS_BODY;
        }
        if params.is_some_and(|p| self.variadic(p)) {
            flags |= sig::VARIADIC;
        }
        let h = &mut self.out.head;
        h[head::SYNTAX] = f.kind as u32;
        h[head::FLAGS] = flags;
        let set_list = |h: &mut crate::columns::Head, word: usize, l: Option<List>| {
            if let Some(l) = l {
                h[word] = l.start;
                h[word + 1] = l.len;
            }
        };
        set_list(h, head::ATTRS, Some(attrs));
        set_list(h, head::GENERICS, generics);
        set_list(h, head::WHERE, where_);
        set_list(h, head::PARAMS, params);
        set_list(h, head::BOUNDS, bounds);
        set_list(h, head::ITEMS, items);
        if let Some((l, v)) = fields {
            set_list(h, head::FIELDS, Some(l));
            h[head::SHAPE] = match v {
                V::List(L::Fields(s)) => shape_bits(s) as u32,
                _ => NONE,
            };
        } else if matches!(f.kind, STRUCT | UNION) {
            h[head::SHAPE] = shape_bits(Shape::Unit) as u32;
        }
        h[head::NAME] = name;
        let (vis_code, vis_path) = vis.encode();
        h[head::VIS] = vis_code as u32;
        h[head::VIS + 1] = vis_path;
        if let Some(abi) = abi {
            (h[head::ABI], h[head::ABI + 1]) =
                if abi.node.a == 1 { (abi.range.start, abi.range.end) } else { (NONE - 1, NONE - 1) };
        }
        match f.kind {
            FN => h[head::TY] = opt(ret),
            IMPL if has_for => {
                h[head::TRAIT] = opt(t0);
                h[head::SELF_TY] = opt(t1);
            }
            IMPL => h[head::SELF_TY] = opt(t0),
            TYPE_ALIAS | CONST | STATIC => h[head::TY] = opt(t0),
            _ => {}
        }
        // `#[repr(..)]`: the row of the attribute, read by layout.
        for i in attrs.start..attrs.start + attrs.len {
            if let Ok(a) = Attr::decode(self.out.rows[i as usize], Lens::BUILDING)
                && a.path.raw().of(self.text()) == "repr"
            {
                self.out.head[head::REPR] = i;
            }
        }
    }

    fn body_root(&mut self, f: &Frame, r: Range) {
        self.out.head[head::SYNTAX] = f.kind as u32;
        match f.kind {
            FN => {
                let params = self.list_of(f, L::Params).map(|l| l.0);
                let body = self.first(f, V::Expr);
                let ret = self.first(f, V::Ret).map_or(NONE, |v| v.id);
                self.done(f);
                if f.flags & F_ASYNC != 0 {
                    self.out.head[head::FLAGS] = sig::ASYNC;
                    self.out.head[head::TY] = ret;
                }
                let Some(body) = body else { return };
                let params = params.unwrap_or(List { start: 0, len: 0 });
                let mut pats: AVec<'_, u32> = Vec::new_in(self.vals.allocator().clone());
                for i in params.start..params.start + params.len {
                    if let Ok(p) = Param::decode(self.out.rows[i as usize]) {
                        pats.push(opt(p.pat.map(|p| p.0)));
                    }
                }
                let (root, pats) = if f.flags & F_ASYNC != 0 {
                    self.async_fn(&pats, ExprId(body.id), r)
                } else {
                    (ExprId(body.id), self.put_ids_of(&pats))
                };
                self.out.head[head::ROOT] = root.0;
                self.out.head[head::PARAMS] = pats.start;
                self.out.head[head::PARAMS + 1] = pats.len;
            }
            CONST | STATIC => {
                let body = self.first(f, V::Expr);
                self.done(f);
                if let Some(body) = body {
                    self.out.head[head::ROOT] = body.id;
                }
            }
            _ => self.done(f),
        }
    }
}
