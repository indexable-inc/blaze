//! A program's source files to signatures and bodies in blaze-typeck's body form, straight from
//! the parser's events: no syntax tree. Stands in for crates/blaze-lower until that answers, so
//! it is the minimum the ladder's programs need (crates/blaze-cli/tests/ladder/README.md).
//!
//! Each file is lexed and parsed once and its event stream walked with a stack of the open
//! nodes and, beside it, a stack of finished child values: a node's value is made when it
//! closes, from its children's (bottom-up, as the events come). Locals are resolved here, by
//! lexical scope with shadowing (`Expr::Local`); every other value path is kept as written and
//! resolved by the world ([`crate::world`]) in its function's module. Types written in
//! signatures and fields go to the program's one type table; types written in a body to the
//! body's. A type path naming a struct is resolved once every file is walked.
//!
//! `while c { b }` is `loop { if c { b } else { break } }`. An `unsafe { .. }` block is a block.
//!
//! Everything outside the subset is a [`LowerError`] naming it and its place, never skipped
//! (AGENTS.md rule 3). Not enforced, as the subset needs neither: privacy (`pub` is read and
//! not checked) and the split of the type and value namespaces (one name per module).

use core::fmt;
use std::alloc::Global;
use std::boxed::Box;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_typeck::{
    Adt, AdtKind, Arm, BinOp, BindAnn, BodyPath, BodyView, CtorKind, Edition, Expr, ExprId, Field, FieldInit, FnSig,
    IntSuffix, IntTy, ItemKey, LangItem, List, Lit, Mutbl, Name, PNode, PTy, PTys, Pat, PatId, PathIdx, Range,
    ScopeId, Segment, StableHasher, Stmt, StmtId, UintTy, UnOp, Variant,
};
use parser::{LexedStr, OutlineStep, Step, SyntaxKind};
use parser::SyntaxKind::*;

/// The edition the ladder's programs are parsed and checked in.
const PARSE_EDITION: parser::Edition = parser::Edition::Edition2024;
const CHECK_EDITION: Edition = Edition::E2024;

/// No token yet.
const NONE: u32 = u32::MAX;

/// One source file of the program.
pub struct SourceFile<'t> {
    /// Relative to the package root: "src/main.rs".
    pub path: &'t str,
    pub text: &'t str,
}

/// A function's calling convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Abi {
    Rust,
    C,
}

/// A fn item: a definition with a body, or an extern block's declaration.
pub struct FnItem {
    pub key: ItemKey,
    /// Its path from the crate root: "util::f".
    pub path: Box<str, Global>,
    pub abi: Abi,
    pub no_mangle: bool,
    pub is_unsafe: bool,
    /// `None`: an extern block's declaration.
    pub body: Option<Body>,
    /// Its file, by index in the files given to [`lower`].
    pub file: u32,
    /// The byte offset of the item in its file (its first attribute's, if any).
    pub at: u32,
    /// Its module, by index in [`Program::modules`].
    pub module: u32,
    /// `#[link_name = ".."]`.
    pub link_name: Option<Box<str, Global>>,
    name: Box<str, Global>,
    inputs: List,
    output: PTy,
}

impl FnItem {
    /// The outside symbol an extern declaration binds to: its `link_name` if written, else its
    /// own name.
    pub fn symbol(&self) -> Box<str, Global> {
        match &self.link_name {
            Some(name) => name.clone(),
            None => self.name.clone(),
        }
    }
}

/// A braced struct.
pub struct StructItem {
    pub key: ItemKey,
    pub path: Box<str, Global>,
    pub file: u32,
    pub at: u32,
    variants: Vec<Variant, Global>,
    fields: Vec<Field, Global>,
}

/// A trait: only an empty lang-item trait is in the subset.
pub struct TraitItem {
    pub key: ItemKey,
    pub path: Box<str, Global>,
}

/// A module: the crate root (index 0) or a `mod name;` and its file.
pub struct Module {
    /// "" for the crate root.
    pub path: Box<str, Global>,
    pub parent: Option<u32>,
    pub file: u32,
}

/// What a name in a module is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ItemRef {
    Fn(u32),
    Struct(u32),
    Trait(u32),
    Mod(u32),
}

/// A lowered program.
pub struct Program {
    pub fns: Vec<FnItem, Global>,
    pub structs: Vec<StructItem, Global>,
    pub traits: Vec<TraitItem, Global>,
    pub modules: Vec<Module, Global>,
    /// `#![no_core]` and `#![no_main]` at the crate root.
    pub no_core: bool,
    pub no_main: bool,
    pub(crate) lang: Vec<(LangItem, ItemKey), Global>,
    pub(crate) names: HashMap<(u32, Name), ItemRef, Global>,
    by_key: HashMap<ItemKey, ItemRef, Global>,
    /// The signature and field type table.
    sig_nodes: Vec<PNode, Global>,
    sig_lists: Vec<PTy, Global>,
    sig_inputs: Vec<PTy, Global>,
}

/// One body's owned columns, and the literal values the body form does not carry.
pub struct Body {
    edition: Edition,
    params: Vec<PatId, Global>,
    root: ExprId,
    exprs: Vec<Expr, Global>,
    expr_ranges: Vec<Range, Global>,
    pats: Vec<Pat, Global>,
    pat_ranges: Vec<Range, Global>,
    stmts: Vec<Stmt, Global>,
    expr_lists: Vec<ExprId, Global>,
    pat_lists: Vec<PatId, Global>,
    stmt_lists: Vec<StmtId, Global>,
    arms: Vec<Arm, Global>,
    field_inits: Vec<FieldInit, Global>,
    paths: Vec<BodyPath, Global>,
    segments: Vec<Segment, Global>,
    ty_nodes: Vec<PNode, Global>,
    ty_lists: Vec<PTy, Global>,
    /// Per expression: an integer, byte or char literal's value.
    expr_values: Vec<u128, Global>,
    /// Per pattern: a literal pattern's value (its magnitude for `-lit`).
    pat_values: Vec<u128, Global>,
    /// String and byte string literals' bytes, by expression.
    byte_strs: Vec<(ExprId, u32, u32), Global>,
    bytes: Vec<u8, Global>,
}

impl Body {
    /// The body as the checker reads it.
    pub fn view(&self, owner: ItemKey) -> BodyView<'_> {
        BodyView {
            owner,
            edition: self.edition,
            params: &self.params,
            root: self.root,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &self.expr_lists,
            pat_lists: &self.pat_lists,
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &self.field_inits,
            field_pats: &[],
            paths: &self.paths,
            segments: &self.segments,
            types: PTys { nodes: &self.ty_nodes, lists: &self.ty_lists },
            type_lists: &[],
            opt_types: &[],
        }
    }

    /// The value of an integer, byte or char literal expression.
    pub fn int_value(&self, e: ExprId) -> Option<u128> {
        match self.exprs.get(e.ix())? {
            Expr::Lit(Lit::Int(_) | Lit::Char) => self.expr_values.get(e.ix()).copied(),
            _ => None,
        }
    }

    /// The value of an integer or char literal pattern (its magnitude for `-lit`).
    pub fn pat_int_value(&self, p: PatId) -> Option<u128> {
        match self.pats.get(p.ix())? {
            Pat::Lit(Lit::Int(_) | Lit::Char) | Pat::NegLit(Lit::Int(_)) => self.pat_values.get(p.ix()).copied(),
            _ => None,
        }
    }

    /// The bytes of a string or byte string literal expression.
    pub fn byte_str(&self, e: ExprId) -> Option<&[u8]> {
        let &(_, start, len) = self.byte_strs.iter().find(|(x, _, _)| *x == e)?;
        self.bytes.get(start as usize..(start + len) as usize)
    }
}

impl Program {
    /// Fn `f`'s signature, in the program's type table.
    pub fn sig(&self, f: usize) -> FnSig<'_> {
        let item = &self.fns[f];
        FnSig {
            parent_count: 0,
            own_count: 0,
            self_param: false,
            inputs: &self.sig_inputs[item.inputs.range()],
            output: item.output,
            types: self.types(),
        }
    }

    /// The signature and field type table.
    pub(crate) fn types(&self) -> PTys<'_> {
        PTys { nodes: &self.sig_nodes, lists: &self.sig_lists }
    }

    /// The crate root's `main`.
    pub fn entry(&self) -> Option<usize> {
        match self.names.get(&(0, Name::of("main"))) {
            Some(&ItemRef::Fn(i)) => Some(i as usize),
            _ => None,
        }
    }

    /// The checker's world for fn `f`'s body: paths resolve in its module.
    pub fn world(&self, f: usize) -> crate::world::BodyWorld<'_> {
        crate::world::BodyWorld { program: self, module: self.fns[f].module }
    }

    /// The struct `key`'s definition.
    pub fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let &ItemRef::Struct(i) = self.by_key.get(&key)? else { return None };
        let s = &self.structs[i as usize];
        Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &s.variants, fields: &s.fields, types: self.types() })
    }

    /// A fn item's index in `fns`.
    pub fn index_of(&self, key: ItemKey) -> Option<usize> {
        match self.by_key.get(&key)? {
            &ItemRef::Fn(i) => Some(i as usize),
            _ => None,
        }
    }

    pub(crate) fn item(&self, key: ItemKey) -> Option<ItemRef> {
        self.by_key.get(&key).copied()
    }

    pub(crate) fn key_of(&self, r: ItemRef) -> Option<ItemKey> {
        match r {
            ItemRef::Fn(i) => Some(self.fns[i as usize].key),
            ItemRef::Struct(i) => Some(self.structs[i as usize].key),
            ItemRef::Trait(i) => Some(self.traits[i as usize].key),
            ItemRef::Mod(_) => None,
        }
    }

    /// The item a path names from `module`, and how many segments it took: `crate`, `self`
    /// and `super` lead, modules are walked, and the first non-module item ends the walk.
    pub(crate) fn resolve(&self, module: u32, segs: &[Name]) -> Option<(ItemRef, u32)> {
        let (krate, this, sup) = (Name::of("crate"), Name::of("self"), Name::of("super"));
        let mut cur = module;
        let mut leading = true;
        for (i, &s) in segs.iter().enumerate() {
            let last = i + 1 == segs.len();
            let step = if s == krate && i == 0 {
                Some(0)
            } else if s == this && i == 0 {
                Some(cur)
            } else if s == sup && leading {
                Some(self.modules[cur as usize].parent?)
            } else {
                None
            };
            if let Some(m) = step {
                cur = m;
                if last {
                    return Some((ItemRef::Mod(m), i as u32 + 1));
                }
                continue;
            }
            leading = false;
            let r = *self.names.get(&(cur, s))?;
            match r {
                ItemRef::Mod(m) if !last => cur = m,
                _ => return Some((r, i as u32 + 1)),
            }
        }
        None
    }
}

/// Why a program was not lowered. Positions are byte offsets in file `file` (an index into the
/// files given to [`lower`]); [`LowerError::located`] prints them as `path:line:col`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// A construct outside the ladder's subset.
    Unsupported { file: u32, at: u32, what: &'static str },
    /// A syntax node outside the ladder's subset, by its parser kind.
    UnsupportedSyntax { file: u32, at: u32, kind: SyntaxKind },
    /// The lexer or the parser refused the text.
    Syntax { file: u32, at: u32 },
    /// `mod name;` with neither `name.rs` nor `name/mod.rs` among the files.
    NoModuleFile { file: u32, at: u32, name: Box<str, Global> },
    /// `mod name;` with both `name.rs` and `name/mod.rs`.
    AmbiguousModuleFile { file: u32, at: u32, name: Box<str, Global> },
    /// A file is the module file of two `mod` items.
    ModuleFileTwice { file: u32, at: u32, name: Box<str, Global> },
    /// The crate root is not among the files.
    NoRootFile { path: Box<str, Global> },
    /// A second item of one name in one module.
    Duplicate { file: u32, at: u32, name: Box<str, Global> },
    /// A type path that names nothing.
    UnresolvedType { file: u32, at: u32, name: Box<str, Global> },
    /// A type path that names an item that is not a struct.
    NotAType { file: u32, at: u32, name: Box<str, Global> },
}

impl LowerError {
    fn place(&self) -> Option<(u32, u32)> {
        match *self {
            LowerError::Unsupported { file, at, .. }
            | LowerError::UnsupportedSyntax { file, at, .. }
            | LowerError::Syntax { file, at }
            | LowerError::NoModuleFile { file, at, .. }
            | LowerError::AmbiguousModuleFile { file, at, .. }
            | LowerError::ModuleFileTwice { file, at, .. }
            | LowerError::Duplicate { file, at, .. }
            | LowerError::UnresolvedType { file, at, .. }
            | LowerError::NotAType { file, at, .. } => Some((file, at)),
            LowerError::NoRootFile { .. } => None,
        }
    }

    fn what(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LowerError::Unsupported { what, .. } => write!(f, "{what} is outside the ladder's subset"),
            LowerError::UnsupportedSyntax { kind, .. } => write!(f, "syntax {kind:?} is outside the ladder's subset"),
            LowerError::Syntax { .. } => write!(f, "a syntax error"),
            LowerError::NoModuleFile { name, .. } => write!(f, "`mod {name};` has no file `{name}.rs` or `{name}/mod.rs`"),
            LowerError::AmbiguousModuleFile { name, .. } => {
                write!(f, "`mod {name};` has both `{name}.rs` and `{name}/mod.rs`")
            }
            LowerError::ModuleFileTwice { name, .. } => write!(f, "`mod {name};` names a file already a module"),
            LowerError::NoRootFile { path } => write!(f, "the crate root `{path}` is not among the files"),
            LowerError::Duplicate { name, .. } => write!(f, "a second item named `{name}` in one module"),
            LowerError::UnresolvedType { name, .. } => write!(f, "the type `{name}` names nothing"),
            LowerError::NotAType { name, .. } => write!(f, "`{name}` is not a struct or a primitive type"),
        }
    }

    /// The error with its place as `path:line:col` in `files`.
    pub fn located<'e>(&'e self, files: &'e [SourceFile<'e>]) -> Located<'e> {
        Located { error: self, files }
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((file, at)) = self.place() {
            write!(f, "file #{file} byte {at}: ")?;
        }
        self.what(f)
    }
}

impl core::error::Error for LowerError {}

/// A [`LowerError`] printed with `path:line:col`.
pub struct Located<'e> {
    error: &'e LowerError,
    files: &'e [SourceFile<'e>],
}

impl fmt::Display for Located<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((file, at)) = self.error.place() {
            match self.files.get(file as usize) {
                Some(sf) => {
                    let before = sf.text.get(..at as usize).unwrap_or(sf.text);
                    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
                    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
                    write!(f, "{}:{line}:{col}: ", sf.path)?;
                }
                None => write!(f, "file #{file} byte {at}: ")?,
            }
        }
        self.error.what(f)
    }
}

/// The key of an item: its module's path, its kind and its name, never its position.
fn item_key(module: &str, kind: u8, name: &str) -> ItemKey {
    let part = |domain: &str| {
        StableHasher::new(domain).bytes(module.as_bytes()).byte(0xff).byte(kind).bytes(name.as_bytes()).finish()
    };
    ItemKey((u128::from(part("blaze-ladder item hi")) << 64) | u128::from(part("blaze-ladder item lo")))
}

fn name_of(text: &str) -> Name {
    Name::of(text.strip_prefix("r#").unwrap_or(text))
}

fn boxed(text: &str) -> Box<str, Global> {
    Box::from(text)
}

/// Lowers the program whose crate root is `root` (a path among `files`).
pub fn lower(files: &[SourceFile<'_>], root: &str) -> Result<Program, LowerError> {
    let Some(root_file) = files.iter().position(|f| f.path == root) else {
        return Err(LowerError::NoRootFile { path: boxed(root) });
    };
    let mut program = Program {
        fns: Vec::new_in(Global),
        structs: Vec::new_in(Global),
        traits: Vec::new_in(Global),
        modules: Vec::new_in(Global),
        no_core: false,
        no_main: false,
        lang: Vec::new_in(Global),
        names: HashMap::new_in(Global),
        by_key: HashMap::new_in(Global),
        sig_nodes: Vec::new_in(Global),
        sig_lists: Vec::new_in(Global),
        sig_inputs: Vec::new_in(Global),
    };
    program.modules.push(Module { path: boxed(""), parent: None, file: root_file as u32 });
    let mut fixups: Vec<Fixup, Global> = Vec::new_in(Global);
    // Modules in the order their `mod` items were met; each is walked once.
    let mut next = 0;
    while next < program.modules.len() {
        let module = next as u32;
        next += 1;
        let file = program.modules[module as usize].file;
        walk_file(&mut program, &mut fixups, files, root_file as u32, file, module)?;
    }
    for fix in fixups.iter() {
        let res = program.resolve(fix.module, &fix.names);
        let key = match res {
            Some((ItemRef::Struct(i), n)) if n as usize == fix.names.len() => program.structs[i as usize].key,
            Some(_) => return Err(LowerError::NotAType { file: fix.file, at: fix.at, name: fix.text.clone() }),
            None => return Err(LowerError::UnresolvedType { file: fix.file, at: fix.at, name: fix.text.clone() }),
        };
        let node = PNode::Adt(key, List::EMPTY);
        match fix.table {
            None => program.sig_nodes[fix.node as usize] = node,
            Some(f) => {
                let Some(body) = program.fns[f as usize].body.as_mut() else { continue };
                body.ty_nodes[fix.node as usize] = node;
            }
        }
    }
    Ok(program)
}

/// A type path to resolve once every file is walked.
struct Fixup {
    /// `None`: the program's table; `Some(f)`: fn `f`'s body's.
    table: Option<u32>,
    node: u32,
    module: u32,
    names: Vec<Name, Global>,
    text: Box<str, Global>,
    file: u32,
    at: u32,
}

/// A finished child's value, waiting for its parent to close.
#[derive(Clone, Copy)]
enum Val {
    Tok(SyntaxKind, u32, u32),
    Expr(ExprId),
    Pat(PatId),
    Ty(PTy),
    /// Segments `start..start + len` of the walk's segment column, and the path's range.
    Path(u32, u32, u32, u32),
    Seg(u32, u32),
    Name(u32, u32),
    Stmt(StmtId),
    Lit(LitVal),
    Arm(Arm),
    Guard(ExprId),
    FieldInit(FieldInit),
    Attr(Attr),
    Abi(Abi),
    Vis,
    Param(PatId, PTy),
    Field(u32, u32, PTy),
    Args(List),
}

#[derive(Clone, Copy)]
struct LitVal {
    lit: Lit,
    value: u128,
    bytes: Option<(u32, u32)>,
}

#[derive(Clone, Copy)]
struct Attr {
    kind: AttrKind,
    inner: bool,
    at: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrKind {
    Doc,
    Features,
    NoCore,
    NoMain,
    NoMangle,
    Lang(LangItem),
    /// The link name's text range.
    LinkName(u32, u32),
}

/// An open node.
struct Frame {
    kind: SyntaxKind,
    start: u32,
    /// Where its children's values start in `vals`.
    base: usize,
    /// A pending-binding mark (`PARAM`, `LET_STMT`, `MATCH_ARM`) or a reserved loop expression.
    mark: usize,
}

/// The body being lowered: the open fn's.
struct BodyB {
    /// The fn's frame.
    fn_depth: usize,
    /// Inside the fn's block: types go to the body's table.
    in_body: bool,
    body: Body,
    bindings: Vec<(Name, PatId), Global>,
    pending: Vec<(Name, PatId), Global>,
    marks: Vec<usize, Global>,
    scopes: Vec<ScopeId, Global>,
    next_scope: u32,
    loops: Vec<ExprId, Global>,
    fixups: Vec<Fixup, Global>,
}

impl BodyB {
    fn new(fn_depth: usize) -> BodyB {
        BodyB {
            fn_depth,
            in_body: false,
            body: Body {
                edition: CHECK_EDITION,
                params: Vec::new_in(Global),
                root: ExprId(0),
                exprs: Vec::new_in(Global),
                expr_ranges: Vec::new_in(Global),
                pats: Vec::new_in(Global),
                pat_ranges: Vec::new_in(Global),
                stmts: Vec::new_in(Global),
                expr_lists: Vec::new_in(Global),
                pat_lists: Vec::new_in(Global),
                stmt_lists: Vec::new_in(Global),
                arms: Vec::new_in(Global),
                field_inits: Vec::new_in(Global),
                paths: Vec::new_in(Global),
                segments: Vec::new_in(Global),
                ty_nodes: Vec::new_in(Global),
                ty_lists: Vec::new_in(Global),
                expr_values: Vec::new_in(Global),
                pat_values: Vec::new_in(Global),
                byte_strs: Vec::new_in(Global),
                bytes: Vec::new_in(Global),
            },
            bindings: Vec::new_in(Global),
            pending: Vec::new_in(Global),
            marks: Vec::new_in(Global),
            scopes: Vec::new_in(Global),
            next_scope: 0,
            loops: Vec::new_in(Global),
            fixups: Vec::new_in(Global),
        }
    }
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).expect("a body's columns are counted in u32")
}

fn list_of<T: Copy>(column: &mut Vec<T, Global>, items: impl Iterator<Item = T>) -> List {
    let start = len32(column.len());
    column.extend(items);
    List { start, len: len32(column.len()) - start }
}

struct Walk<'w, 't> {
    program: &'w mut Program,
    fixups: &'w mut Vec<Fixup, Global>,
    files: &'w [SourceFile<'t>],
    text: &'t str,
    lexed: &'w LexedStr<'t>,
    file: u32,
    module: u32,
    is_root: bool,
    pos: usize,
    offset: u32,
    prev_end: u32,
    frames: Vec<Frame, Global>,
    vals: Vec<Val, Global>,
    /// The depth of the attribute or visibility whose subtree is kept as tokens.
    raw: Option<usize>,
    /// Path segments' text ranges.
    segs: Vec<(u32, u32), Global>,
    body: Option<BodyB>,
    extern_abi: Option<Abi>,
}

fn walk_file(
    program: &mut Program,
    fixups: &mut Vec<Fixup, Global>,
    files: &[SourceFile<'_>],
    root_file: u32,
    file: u32,
    module: u32,
) -> Result<(), LowerError> {
    let text = files[file as usize].text;
    if u32::try_from(text.len()).is_err() {
        return Err(LowerError::Unsupported { file, at: 0, what: "a source file of 4 GiB or more" });
    }
    let lexed = LexedStr::new(PARSE_EDITION, text);
    if let Some((token, _)) = lexed.errors().next() {
        return Err(LowerError::Syntax { file, at: lexed.text_range(token).start as u32 });
    }
    let input = lexed.to_input(PARSE_EDITION);
    let output = parser::TopEntryPoint::SourceFile.parse(input.tokens());
    let mut w = Walk {
        program,
        fixups,
        files,
        text,
        lexed: &lexed,
        file,
        module,
        is_root: file == root_file,
        pos: 0,
        offset: 0,
        prev_end: 0,
        frames: Vec::new_in(Global),
        vals: Vec::new_in(Global),
        raw: None,
        segs: Vec::new_in(Global),
        body: None,
        extern_abi: None,
    };
    for step in output.steps() {
        match step {
            OutlineStep::Step(Step::Token { kind, n_input_tokens }) => w.token(kind, n_input_tokens)?,
            OutlineStep::Step(Step::FloatSplit { .. }) => {
                let at = w.here();
                return Err(w.unsupported(at, "a tuple field access"));
            }
            OutlineStep::Step(Step::Enter { kind }) => w.enter(kind)?,
            OutlineStep::Step(Step::Exit) => w.exit()?,
            OutlineStep::Step(Step::Error { .. }) => return Err(LowerError::Syntax { file, at: w.here() }),
            OutlineStep::Lazy { .. } => {
                let at = w.here();
                return Err(w.unsupported(at, "a node the parser left unparsed"));
            }
        }
    }
    Ok(())
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

fn primitive(name: &str) -> Option<PNode> {
    Some(match name {
        "i8" => PNode::Int(IntTy::I8),
        "i16" => PNode::Int(IntTy::I16),
        "i32" => PNode::Int(IntTy::I32),
        "i64" => PNode::Int(IntTy::I64),
        "i128" => PNode::Int(IntTy::I128),
        "isize" => PNode::Int(IntTy::Isize),
        "u8" => PNode::Uint(UintTy::U8),
        "u16" => PNode::Uint(UintTy::U16),
        "u32" => PNode::Uint(UintTy::U32),
        "u64" => PNode::Uint(UintTy::U64),
        "u128" => PNode::Uint(UintTy::U128),
        "usize" => PNode::Uint(UintTy::Usize),
        "bool" => PNode::Bool,
        "char" => PNode::Char,
        "str" => PNode::Str,
        _ => return None,
    })
}

fn int_suffix(text: &str) -> (&str, Option<IntSuffix>) {
    const SUFFIXES: [(&str, IntSuffix); 12] = [
        ("i128", IntSuffix::Int(IntTy::I128)),
        ("isize", IntSuffix::Int(IntTy::Isize)),
        ("i16", IntSuffix::Int(IntTy::I16)),
        ("i32", IntSuffix::Int(IntTy::I32)),
        ("i64", IntSuffix::Int(IntTy::I64)),
        ("i8", IntSuffix::Int(IntTy::I8)),
        ("u128", IntSuffix::Uint(UintTy::U128)),
        ("usize", IntSuffix::Uint(UintTy::Usize)),
        ("u16", IntSuffix::Uint(UintTy::U16)),
        ("u32", IntSuffix::Uint(UintTy::U32)),
        ("u64", IntSuffix::Uint(UintTy::U64)),
        ("u8", IntSuffix::Uint(UintTy::U8)),
    ];
    for (s, suffix) in SUFFIXES {
        if let Some(digits) = text.strip_suffix(s) {
            if !digits.is_empty() {
                return (digits.strip_suffix('_').unwrap_or(digits), Some(suffix));
            }
        }
    }
    (text, None)
}

fn lang_item(name: &str) -> Option<LangItem> {
    Some(match name {
        "sized" => LangItem::Sized,
        "copy" => LangItem::Copy,
        "deref" => LangItem::Deref,
        "deref_mut" => LangItem::DerefMut,
        "add" => LangItem::Add,
        "sub" => LangItem::Sub,
        "mul" => LangItem::Mul,
        "div" => LangItem::Div,
        "rem" => LangItem::Rem,
        "bitand" => LangItem::BitAnd,
        "bitor" => LangItem::BitOr,
        "bitxor" => LangItem::BitXor,
        "shl" => LangItem::Shl,
        "shr" => LangItem::Shr,
        "add_assign" => LangItem::AddAssign,
        "sub_assign" => LangItem::SubAssign,
        "mul_assign" => LangItem::MulAssign,
        "div_assign" => LangItem::DivAssign,
        "rem_assign" => LangItem::RemAssign,
        "bitand_assign" => LangItem::BitAndAssign,
        "bitor_assign" => LangItem::BitOrAssign,
        "bitxor_assign" => LangItem::BitXorAssign,
        "shl_assign" => LangItem::ShlAssign,
        "shr_assign" => LangItem::ShrAssign,
        "neg" => LangItem::Neg,
        "not" => LangItem::Not,
        "eq" => LangItem::PartialEq,
        "partial_ord" => LangItem::PartialOrd,
        "index" => LangItem::Index,
        "index_mut" => LangItem::IndexMut,
        "fn_once" => LangItem::FnOnce,
        "fn_mut" => LangItem::FnMut,
        "fn" => LangItem::Fn,
        _ => return None,
    })
}

/// A quoted literal's contents unescaped into `out`; `None` for an escape outside the subset.
fn unescape(inner: &str, out: &mut Vec<u8, Global>) -> Option<()> {
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next()? {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '0' => out.push(0),
            '\\' => out.push(b'\\'),
            '\'' => out.push(b'\''),
            '"' => out.push(b'"'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
            }
            '\n' => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            _ => return None,
        }
    }
    Some(())
}

impl<'w, 't> Walk<'w, 't> {
    fn unsupported(&self, at: u32, what: &'static str) -> LowerError {
        LowerError::Unsupported { file: self.file, at, what }
    }

    fn skip_trivia(&mut self) {
        while self.pos < self.lexed.len() && self.lexed.kind(self.pos).is_trivia() {
            self.offset += self.lexed.text_len(self.pos) as u32;
            self.pos += 1;
        }
    }

    /// Where the next token starts.
    fn here(&mut self) -> u32 {
        self.skip_trivia();
        self.offset
    }

    fn text(&self, s: u32, e: u32) -> &'t str {
        let text: &'t str = self.text;
        &text[s as usize..e as usize]
    }

    fn token(&mut self, kind: SyntaxKind, n: u8) -> Result<(), LowerError> {
        let start = self.here();
        let mut end = start;
        for _ in 0..n {
            self.skip_trivia();
            if self.pos >= self.lexed.len() {
                return Err(LowerError::Syntax { file: self.file, at: self.offset });
            }
            end = self.offset + self.lexed.text_len(self.pos) as u32;
            self.offset = end;
            self.pos += 1;
        }
        for frame in self.frames.iter_mut().rev() {
            if frame.start != NONE {
                break;
            }
            frame.start = start;
        }
        self.prev_end = end;
        if !self.frames.is_empty() {
            self.vals.push(Val::Tok(kind, start, end));
        }
        Ok(())
    }

    fn b(&mut self, at: u32) -> Result<&mut BodyB, LowerError> {
        let file = self.file;
        self.body.as_mut().ok_or(LowerError::Unsupported { file, at, what: "an expression or pattern outside a fn" })
    }

    fn enter(&mut self, kind: SyntaxKind) -> Result<(), LowerError> {
        let at = self.here();
        let parent = self.frames.last().map(|f| f.kind);
        let depth = self.frames.len();
        let mut mark = 0;
        if self.raw.is_none() {
            let is_item = matches!(kind, FN | STRUCT | TRAIT | MODULE | EXTERN_BLOCK);
            if is_item && self.body.is_some() {
                return Err(self.unsupported(at, "an item inside a fn body"));
            }
            match kind {
                ATTR | DOC_COMMENT | VISIBILITY => self.raw = Some(depth),
                SOURCE_FILE | NAME | NAME_REF | PATH | PATH_SEGMENT | PATH_EXPR | PATH_TYPE | PTR_TYPE | REF_TYPE
                | TUPLE_TYPE | NEVER_TYPE | INFER_TYPE | ABI | PARAM_LIST | RET_TYPE | EXPR_STMT | LITERAL
                | BIN_EXPR | PREFIX_EXPR | PAREN_EXPR | TUPLE_EXPR | REF_EXPR | CAST_EXPR | CALL_EXPR | ARG_LIST
                | FIELD_EXPR | RECORD_EXPR | RECORD_EXPR_FIELD_LIST | RECORD_EXPR_FIELD | IF_EXPR | BREAK_EXPR
                | CONTINUE_EXPR | RETURN_EXPR | MATCH_EXPR | MATCH_ARM_LIST | MATCH_GUARD | WILDCARD_PAT
                | IDENT_PAT | LITERAL_PAT | OR_PAT | RECORD_FIELD_LIST | RECORD_FIELD | ASSOC_ITEM_LIST
                | EXTERN_ITEM_LIST | BLOCK_EXPR => {}
                STRUCT | TRAIT | MODULE | EXTERN_BLOCK => {
                    if parent != Some(SOURCE_FILE) {
                        return Err(self.unsupported(at, "an item that is not at a file's top level"));
                    }
                }
                FN => {
                    match parent {
                        Some(SOURCE_FILE | EXTERN_ITEM_LIST) => {}
                        Some(ASSOC_ITEM_LIST) => return Err(self.unsupported(at, "a fn in a trait or impl")),
                        _ => return Err(self.unsupported(at, "a fn that is not at a file's top level")),
                    }
                    self.body = Some(BodyB::new(depth));
                }
                PARAM | LET_STMT | MATCH_ARM => mark = self.b(at)?.pending.len(),
                STMT_LIST => {
                    let b = self.b(at)?;
                    b.marks.push(b.bindings.len());
                    let scope = ScopeId(b.next_scope);
                    b.next_scope += 1;
                    b.scopes.push(scope);
                }
                WHILE_EXPR | LOOP_EXPR => {
                    let b = self.b(at)?;
                    let id = ExprId(len32(b.body.exprs.len()));
                    b.body.exprs.push(Expr::Missing);
                    b.body.expr_ranges.push(Range::default());
                    b.body.expr_values.push(0);
                    b.loops.push(id);
                    mark = id.ix();
                }
                LET_EXPR => return Err(self.unsupported(at, "`if let` or `while let`")),
                CLOSURE_EXPR => return Err(self.unsupported(at, "a closure")),
                METHOD_CALL_EXPR => return Err(self.unsupported(at, "a method call")),
                FOR_EXPR => return Err(self.unsupported(at, "a `for` loop")),
                MACRO_CALL | MACRO_EXPR | MACRO_STMTS => return Err(self.unsupported(at, "a macro call")),
                GENERIC_PARAM_LIST | GENERIC_ARG_LIST | WHERE_CLAUSE => {
                    return Err(self.unsupported(at, "generics"));
                }
                IMPL => return Err(self.unsupported(at, "an impl")),
                USE => return Err(self.unsupported(at, "a `use` item")),
                ITEM_LIST => return Err(self.unsupported(at, "an inline module")),
                LABEL | LIFETIME => return Err(self.unsupported(at, "a label or lifetime")),
                _ => return Err(LowerError::UnsupportedSyntax { file: self.file, at, kind }),
            }
            if kind == BLOCK_EXPR && parent == Some(FN) {
                self.b(at)?.in_body = true;
            }
        }
        self.frames.push(Frame { kind, start: NONE, base: self.vals.len(), mark });
        Ok(())
    }

    fn exit(&mut self) -> Result<(), LowerError> {
        let Some(frame) = self.frames.pop() else { return Err(LowerError::Syntax { file: self.file, at: self.offset }) };
        let start = if frame.start == NONE { self.offset } else { frame.start };
        let end = self.prev_end.max(start);
        let depth = self.frames.len();
        if let Some(r) = self.raw {
            if r < depth {
                return Ok(());
            }
            self.raw = None;
        }
        let kids = self.vals.split_off(frame.base);
        let parent = self.frames.last().map(|f| f.kind);
        match frame.kind {
            PARAM_LIST | RET_TYPE | MATCH_ARM_LIST | RECORD_EXPR_FIELD_LIST | RECORD_FIELD_LIST | ASSOC_ITEM_LIST
            | EXTERN_ITEM_LIST => {
                for &k in kids.iter() {
                    match k {
                        Val::Tok(..) => {}
                        Val::Vis | Val::Attr(Attr { kind: AttrKind::Doc, .. }) if frame.kind == RECORD_FIELD_LIST => {}
                        other => self.vals.push(other),
                    }
                }
                if frame.kind == RECORD_EXPR_FIELD_LIST && kids.iter().any(|k| matches!(k, Val::Tok(DOT2, ..))) {
                    return Err(self.unsupported(start, "struct update syntax"));
                }
                if frame.kind == ASSOC_ITEM_LIST && self.vals.len() > frame.base {
                    return Err(self.unsupported(start, "an item in a trait"));
                }
                return Ok(());
            }
            _ => {}
        }
        let val = self.lower_node(&frame, parent, &kids, start, end)?;
        if let Some(v) = val {
            if let (Val::Pat(_), Some(MATCH_ARM)) = (v, parent) {
                let arm_mark = self.frames.last().map_or(0, |f| f.mark);
                let b = self.b(start)?;
                b.marks.push(b.bindings.len());
                let moved = b.pending.split_off(arm_mark);
                b.bindings.extend(moved);
            }
            self.vals.push(v);
        }
        Ok(())
    }

    fn rel(&self, s: u32, e: u32) -> Range {
        let base = self.body.as_ref().map_or(0, |b| self.frames.get(b.fn_depth).map_or(0, |f| f.start));
        Range { start: s.saturating_sub(base), end: e.saturating_sub(base) }
    }

    fn expr(&mut self, e: Expr, s: u32, end: u32, value: u128) -> Result<ExprId, LowerError> {
        let r = self.rel(s, end);
        let b = self.b(s)?;
        let id = ExprId(len32(b.body.exprs.len()));
        b.body.exprs.push(e);
        b.body.expr_ranges.push(r);
        b.body.expr_values.push(value);
        Ok(id)
    }

    fn pat(&mut self, p: Pat, s: u32, end: u32, value: u128) -> Result<PatId, LowerError> {
        let r = self.rel(s, end);
        let b = self.b(s)?;
        let id = PatId(len32(b.body.pats.len()));
        b.body.pats.push(p);
        b.body.pat_ranges.push(r);
        b.body.pat_values.push(value);
        Ok(id)
    }

    /// A type node in the table of where the walk is: the body's or the program's.
    fn ty(&mut self, n: PNode) -> PTy {
        match self.body.as_mut().filter(|b| b.in_body) {
            Some(b) => {
                b.body.ty_nodes.push(n);
                PTy(len32(b.body.ty_nodes.len() - 1))
            }
            None => {
                self.program.sig_nodes.push(n);
                PTy(len32(self.program.sig_nodes.len() - 1))
            }
        }
    }

    fn ty_list(&mut self, tys: impl Iterator<Item = PTy>) -> List {
        match self.body.as_mut().filter(|b| b.in_body) {
            Some(b) => list_of(&mut b.body.ty_lists, tys),
            None => list_of(&mut self.program.sig_lists, tys),
        }
    }

    fn lit(&mut self, kind: SyntaxKind, s: u32, e: u32) -> Result<LitVal, LowerError> {
        let text = self.text(s, e);
        let plain = |lit, value| Ok(LitVal { lit, value, bytes: None });
        match kind {
            TRUE_KW => plain(Lit::Bool(true), 1),
            FALSE_KW => plain(Lit::Bool(false), 0),
            INT_NUMBER => {
                let (digits, suffix) = int_suffix(text);
                let (radix, digits) = match digits.get(..2) {
                    Some("0x") => (16, &digits[2..]),
                    Some("0o") => (8, &digits[2..]),
                    Some("0b") => (2, &digits[2..]),
                    _ => (10, digits),
                };
                let mut value: u128 = 0;
                for c in digits.chars().filter(|&c| c != '_') {
                    let d = c.to_digit(radix).ok_or(self.unsupported(s, "an integer literal with this suffix"))?;
                    value = value
                        .checked_mul(u128::from(radix))
                        .and_then(|v| v.checked_add(u128::from(d)))
                        .ok_or(self.unsupported(s, "an integer literal past u128"))?;
                }
                plain(Lit::Int(suffix), value)
            }
            CHAR | BYTE => {
                let inner = text.strip_prefix('b').unwrap_or(text);
                let inner = inner.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')).unwrap_or(inner);
                let mut out = Vec::new_in(Global);
                unescape(inner, &mut out).ok_or(self.unsupported(s, "this escape in a literal"))?;
                let value = match core::str::from_utf8(&out).ok().and_then(|t| t.chars().next()) {
                    Some(c) if kind == CHAR => u128::from(u32::from(c)),
                    _ => out.first().map_or(0, |&b| u128::from(b)),
                };
                if kind == CHAR { plain(Lit::Char, value) } else { plain(Lit::Int(Some(IntSuffix::Uint(UintTy::U8))), value) }
            }
            STRING | BYTE_STRING => {
                let byte = kind == BYTE_STRING;
                let inner = if byte { text.strip_prefix('b').unwrap_or(text) } else { text };
                let (raw, inner) = match inner.strip_prefix('r') {
                    Some(rest) => (true, rest.trim_matches('#')),
                    None => (false, inner),
                };
                let inner = inner.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(inner);
                let b = self.b(s)?;
                let at = len32(b.body.bytes.len());
                if raw {
                    b.body.bytes.extend_from_slice(inner.as_bytes());
                } else if unescape(inner, &mut b.body.bytes).is_none() {
                    return Err(self.unsupported(s, "this escape in a literal"));
                }
                let b = self.b(s)?;
                let len = len32(b.body.bytes.len()) - at;
                let lit = if byte { Lit::ByteStr(u64::from(len)) } else { Lit::Str };
                Ok(LitVal { lit, value: 0, bytes: Some((at, len)) })
            }
            FLOAT_NUMBER => Err(self.unsupported(s, "a float literal")),
            _ => Err(self.unsupported(s, "this literal")),
        }
    }

    fn attr(&self, kind: SyntaxKind, kids: &[Val], s: u32) -> Result<Attr, LowerError> {
        if kind == DOC_COMMENT {
            let inner = kids.iter().any(|k| matches!(k, Val::Tok(INNER_DOC_COMMENT, ..)));
            return Ok(Attr { kind: AttrKind::Doc, inner, at: s });
        }
        let toks: Vec<(SyntaxKind, u32, u32), Global> = {
            let mut v = Vec::new_in(Global);
            for &k in kids {
                if let Val::Tok(kind, a, b) = k {
                    v.push((kind, a, b));
                }
            }
            v
        };
        let bad = || LowerError::Unsupported { file: self.file, at: s, what: "this attribute" };
        let inner = toks.get(1).is_some_and(|t| t.0 == BANG);
        let mut i = if inner { 3 } else { 2 };
        let mut end = toks.len().saturating_sub(1);
        if toks.get(i).is_some_and(|t| t.0 == UNSAFE_KW) && toks.get(i + 1).is_some_and(|t| t.0 == L_PAREN) {
            i += 2;
            end = end.saturating_sub(1);
        }
        let body = toks.get(i..end).ok_or_else(bad)?;
        let word = |t: &(SyntaxKind, u32, u32)| self.text(t.1, t.2);
        let string = |t: &(SyntaxKind, u32, u32)| -> Option<(u32, u32)> {
            (t.0 == STRING && t.2 > t.1 + 1).then_some((t.1 + 1, t.2 - 1))
        };
        let kind = match body {
            [name] if word(name) == "no_core" => AttrKind::NoCore,
            [name] if word(name) == "no_main" => AttrKind::NoMain,
            [name] if word(name) == "no_mangle" => AttrKind::NoMangle,
            [name, eq, value] if word(name) == "lang" && eq.0 == EQ => {
                let (a, b) = string(value).ok_or_else(bad)?;
                AttrKind::Lang(lang_item(self.text(a, b)).ok_or(LowerError::Unsupported {
                    file: self.file,
                    at: s,
                    what: "a lang item the checker does not name",
                })?)
            }
            [name, eq, value] if word(name) == "link_name" && eq.0 == EQ => {
                let (a, b) = string(value).ok_or_else(bad)?;
                AttrKind::LinkName(a, b)
            }
            [name, open, rest @ .., close] if word(name) == "feature" && open.0 == L_PAREN && close.0 == R_PAREN => {
                for t in rest {
                    match t.0 {
                        COMMA => {}
                        IDENT if matches!(word(t), "no_core" | "lang_items") => {}
                        _ => {
                            return Err(LowerError::Unsupported {
                                file: self.file,
                                at: t.1,
                                what: "a feature other than `no_core` and `lang_items`",
                            });
                        }
                    }
                }
                AttrKind::Features
            }
            _ => return Err(bad()),
        };
        Ok(Attr { kind, inner, at: s })
    }

    /// A path written in the body: a local when it is one name bound in scope, else a
    /// `BodyPath` the world resolves.
    fn body_path(&mut self, segs: (u32, u32), s: u32) -> Result<Result<PatId, PathIdx>, LowerError> {
        let (first, len) = segs;
        let mut names: Vec<Name, Global> = Vec::new_in(Global);
        for i in first..first + len {
            let (a, b) = self.segs[i as usize];
            names.push(name_of(self.text(a, b)));
        }
        let b = self.b(s)?;
        if len == 1 {
            if let Some(&(_, p)) = b.bindings.iter().rev().find(|(n, _)| *n == names[0]) {
                return Ok(Ok(p));
            }
        }
        let segments = list_of(
            &mut b.body.segments,
            names.iter().map(|&name| Segment { name, generic_args: List::EMPTY, has_args: false }),
        );
        let scope = b.scopes.last().copied().unwrap_or(ScopeId(0));
        let idx = PathIdx(len32(b.body.paths.len()));
        b.body.paths.push(BodyPath { scope, segments });
        Ok(Err(idx))
    }

    fn module_path(&self, name: &str) -> Box<str, Global> {
        let m = &self.program.modules[self.module as usize].path;
        if m.is_empty() {
            boxed(name)
        } else {
            let mut s = std::string::String::new();
            s.push_str(m);
            s.push_str("::");
            s.push_str(name);
            s.into_boxed_str()
        }
    }

    fn register(&mut self, name: &str, r: ItemRef, key: Option<ItemKey>, at: u32) -> Result<(), LowerError> {
        if self.program.names.insert((self.module, name_of(name)), r).is_some() {
            return Err(LowerError::Duplicate { file: self.file, at, name: boxed(name) });
        }
        if let Some(k) = key {
            self.program.by_key.insert(k, r);
        }
        Ok(())
    }

    /// The attributes of an item of `what` kind: each must be one it takes.
    fn item_attrs(&self, kids: &[Val], allowed: &[AttrKind]) -> Result<(), LowerError> {
        for k in kids {
            if let Val::Attr(a) = k {
                let ok = match a.kind {
                    AttrKind::Doc => true,
                    AttrKind::Lang(_) => allowed.iter().any(|x| matches!(x, AttrKind::Lang(_))),
                    AttrKind::LinkName(..) => allowed.iter().any(|x| matches!(x, AttrKind::LinkName(..))),
                    other => allowed.contains(&other),
                };
                if !ok || a.inner {
                    return Err(self.unsupported(a.at, "this attribute on this item"));
                }
            }
        }
        Ok(())
    }

    fn name_in(&self, kids: &[Val], at: u32) -> Result<(u32, u32), LowerError> {
        kids.iter()
            .find_map(|k| match *k {
                Val::Name(a, b) => Some((a, b)),
                _ => None,
            })
            .ok_or(LowerError::Syntax { file: self.file, at })
    }

    fn lower_node(
        &mut self,
        frame: &Frame,
        parent: Option<SyntaxKind>,
        kids: &[Val],
        s: u32,
        e: u32,
    ) -> Result<Option<Val>, LowerError> {
        let exprs = || kids.iter().filter_map(|k| if let Val::Expr(x) = k { Some(*x) } else { None });
        let has = |kind: SyntaxKind| kids.iter().any(|k| matches!(k, Val::Tok(t, ..) if *t == kind));
        let syntax = LowerError::Syntax { file: self.file, at: s };
        Ok(Some(match frame.kind {
            ATTR | DOC_COMMENT => Val::Attr(self.attr(frame.kind, kids, s)?),
            VISIBILITY => Val::Vis,
            NAME | NAME_REF => {
                let Some(&Val::Tok(_, a, b)) = kids.first() else { return Err(syntax) };
                Val::Name(a, b)
            }
            PATH_SEGMENT => {
                let [Val::Name(a, b)] = kids else {
                    return Err(self.unsupported(s, "a path segment with more than a name"));
                };
                self.segs.push((*a, *b));
                Val::Seg(*a, *b)
            }
            PATH => {
                let (first, len) = match kids {
                    [Val::Seg(..)] => (len32(self.segs.len()) - 1, 1),
                    [Val::Path(first, len, ..), Val::Tok(COLON2, ..), Val::Seg(..)] => (*first, len + 1),
                    _ => return Err(self.unsupported(s, "this path (a leading `::` or a qualified path)")),
                };
                Val::Path(first, len, s, e)
            }
            ABI => {
                let abi = match kids {
                    [Val::Tok(EXTERN_KW, ..)] => Abi::C,
                    [Val::Tok(EXTERN_KW, ..), Val::Tok(STRING, a, b)] => match self.text(*a, *b) {
                        "\"C\"" => Abi::C,
                        "\"Rust\"" => Abi::Rust,
                        _ => return Err(self.unsupported(s, "an ABI other than \"C\" and \"Rust\"")),
                    },
                    _ => return Err(syntax),
                };
                if parent == Some(EXTERN_BLOCK) {
                    self.extern_abi = Some(abi);
                    return Ok(None);
                }
                Val::Abi(abi)
            }

            // Types.
            PATH_TYPE => {
                let [Val::Path(first, len, ..)] = kids else { return Err(syntax) };
                let (a, b) = self.segs[*first as usize];
                let one = self.text(a, b);
                match primitive(one).filter(|_| *len == 1) {
                    Some(n) => Val::Ty(self.ty(n)),
                    None => {
                        let mut names = Vec::new_in(Global);
                        for i in *first..first + len {
                            let (a, b) = self.segs[i as usize];
                            names.push(name_of(self.text(a, b)));
                        }
                        let t = self.ty(PNode::Error);
                        let fix = Fixup {
                            table: None,
                            node: t.0,
                            module: self.module,
                            names,
                            text: boxed(self.text(s, e)),
                            file: self.file,
                            at: s,
                        };
                        match self.body.as_mut().filter(|b| b.in_body) {
                            Some(b) => b.fixups.push(fix),
                            None => self.fixups.push(fix),
                        }
                        Val::Ty(t)
                    }
                }
            }
            PTR_TYPE | REF_TYPE => {
                let Some(&Val::Ty(inner)) = kids.last() else { return Err(syntax) };
                let m = if has(MUT_KW) { Mutbl::Mut } else { Mutbl::Not };
                Val::Ty(self.ty(if frame.kind == PTR_TYPE { PNode::Ptr(m, inner) } else { PNode::Ref(m, inner) }))
            }
            TUPLE_TYPE => {
                let tys: Vec<PTy, Global> = {
                    let mut v = Vec::new_in(Global);
                    v.extend(kids.iter().filter_map(|k| if let Val::Ty(t) = k { Some(*t) } else { None }));
                    v
                };
                let l = self.ty_list(tys.iter().copied());
                Val::Ty(self.ty(PNode::Tuple(l)))
            }
            NEVER_TYPE => Val::Ty(self.ty(PNode::Never)),
            INFER_TYPE => {
                if !self.body.as_ref().is_some_and(|b| b.in_body) {
                    return Err(self.unsupported(s, "`_` in a signature"));
                }
                Val::Ty(self.ty(PNode::Infer))
            }

            // Patterns.
            IDENT_PAT => {
                let (a, b) = self.name_in(kids, s)?;
                let sub = kids.iter().find_map(|k| if let Val::Pat(p) = k { Some(*p) } else { None });
                let ann = BindAnn {
                    by_ref: has(REF_KW).then_some(if has(MUT_KW) { Mutbl::Mut } else { Mutbl::Not }),
                    mutable: has(MUT_KW) && !has(REF_KW),
                };
                let name = name_of(self.text(a, b));
                let p = self.pat(Pat::Bind { ann, name, sub }, s, e, 0)?;
                self.b(s)?.pending.push((name, p));
                Val::Pat(p)
            }
            WILDCARD_PAT => Val::Pat(self.pat(Pat::Wild, s, e, 0)?),
            LITERAL_PAT => {
                let Some(&Val::Lit(l)) = kids.last() else { return Err(syntax) };
                if l.bytes.is_some() {
                    return Err(self.unsupported(s, "a string literal pattern"));
                }
                let p = if has(MINUS) { Pat::NegLit(l.lit) } else { Pat::Lit(l.lit) };
                Val::Pat(self.pat(p, s, e, l.value)?)
            }
            OR_PAT => {
                let b = self.b(s)?;
                let alts = list_of(
                    &mut b.body.pat_lists,
                    kids.iter().filter_map(|k| if let Val::Pat(p) = k { Some(*p) } else { None }),
                );
                Val::Pat(self.pat(Pat::Or { alts }, s, e, 0)?)
            }

            // Expressions.
            LITERAL => {
                let Some(&Val::Tok(kind, a, b)) = kids.first() else { return Err(syntax) };
                let l = self.lit(kind, a, b)?;
                if parent == Some(LITERAL_PAT) {
                    Val::Lit(l)
                } else {
                    let x = self.expr(Expr::Lit(l.lit), s, e, l.value)?;
                    if let Some((at, len)) = l.bytes {
                        self.b(s)?.body.byte_strs.push((x, at, len));
                    }
                    Val::Expr(x)
                }
            }
            PATH_EXPR => {
                let [Val::Path(first, len, ..)] = kids else { return Err(syntax) };
                let x = match self.body_path((*first, *len), s)? {
                    Ok(p) => Expr::Local(p),
                    Err(path) => Expr::Path(path),
                };
                Val::Expr(self.expr(x, s, e, 0)?)
            }
            BIN_EXPR => {
                let op = kids.iter().find_map(|k| if let Val::Tok(t, ..) = k { Some(*t) } else { None });
                let mut it = exprs();
                let (Some(op), Some(lhs), Some(rhs), None) = (op, it.next(), it.next(), it.next()) else {
                    return Err(syntax);
                };
                let x = if op == EQ {
                    Expr::Assign { lhs, rhs }
                } else {
                    match bin_op(op) {
                        Some((op, false)) => Expr::Binary { op, lhs, rhs },
                        Some((op, true)) => Expr::AssignOp { op, lhs, rhs },
                        None => return Err(self.unsupported(s, "this binary operator")),
                    }
                };
                Val::Expr(self.expr(x, s, e, 0)?)
            }
            PREFIX_EXPR => {
                let (Some(Val::Tok(t, ..)), Some(operand)) = (kids.first(), exprs().next()) else { return Err(syntax) };
                let op = match t {
                    MINUS => UnOp::Neg,
                    BANG => UnOp::Not,
                    STAR => UnOp::Deref,
                    _ => return Err(self.unsupported(s, "this prefix operator")),
                };
                Val::Expr(self.expr(Expr::Unary { op, operand }, s, e, 0)?)
            }
            REF_EXPR => {
                let Some(operand) = exprs().next() else { return Err(syntax) };
                if has(AMP2) || has(RAW_KW) {
                    return Err(self.unsupported(s, "this borrow (`&&` or `&raw`)"));
                }
                let mutbl = if has(MUT_KW) { Mutbl::Mut } else { Mutbl::Not };
                Val::Expr(self.expr(Expr::Ref { mutbl, operand }, s, e, 0)?)
            }
            PAREN_EXPR => {
                let Some(inner) = exprs().next() else { return Err(syntax) };
                Val::Expr(self.expr(Expr::Paren(inner), s, e, 0)?)
            }
            TUPLE_EXPR => {
                let b = self.b(s)?;
                let elems = list_of(&mut b.body.expr_lists, exprs());
                Val::Expr(self.expr(Expr::Tuple { elems }, s, e, 0)?)
            }
            CAST_EXPR => {
                let (Some(operand), Some(Val::Ty(ty))) = (exprs().next(), kids.last()) else { return Err(syntax) };
                Val::Expr(self.expr(Expr::Cast { operand, ty: *ty }, s, e, 0)?)
            }
            ARG_LIST => {
                let b = self.b(s)?;
                Val::Args(list_of(&mut b.body.expr_lists, exprs()))
            }
            CALL_EXPR => {
                let (Some(callee), Some(Val::Args(args))) = (exprs().next(), kids.last()) else { return Err(syntax) };
                Val::Expr(self.expr(Expr::Call { callee, args: *args }, s, e, 0)?)
            }
            FIELD_EXPR => {
                let (Some(base), Some(Val::Name(a, b))) = (exprs().next(), kids.last()) else { return Err(syntax) };
                let field = self.text(*a, *b);
                if field.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(self.unsupported(s, "a tuple field access"));
                }
                Val::Expr(self.expr(Expr::Field { base, name: name_of(field) }, s, e, 0)?)
            }
            RECORD_EXPR_FIELD => {
                let Some(expr) = exprs().next() else { return Err(syntax) };
                let name = match kids.first() {
                    Some(Val::Name(a, b)) => name_of(self.text(*a, *b)),
                    _ => name_of(self.text(s, e)),
                };
                Val::FieldInit(FieldInit { name, expr })
            }
            RECORD_EXPR => {
                let Some(&Val::Path(first, len, ..)) = kids.first() else { return Err(syntax) };
                let path = match self.body_path((first, len), s)? {
                    Err(p) => p,
                    Ok(_) => return Err(self.unsupported(s, "a struct expression whose path is a local")),
                };
                let b = self.b(s)?;
                let fields = list_of(
                    &mut b.body.field_inits,
                    kids.iter().filter_map(|k| if let Val::FieldInit(f) = k { Some(*f) } else { None }),
                );
                Val::Expr(self.expr(Expr::Struct { path, fields, base: None }, s, e, 0)?)
            }
            STMT_LIST => {
                let b = self.b(s)?;
                let mut stmts: Vec<StmtId, Global> = Vec::new_in(Global);
                let mut tail: Option<ExprId> = None;
                for k in kids {
                    let stmt = match *k {
                        Val::Stmt(st) => Some(st),
                        Val::Expr(x) => {
                            let prev = tail.replace(x);
                            prev.map(|p| {
                                b.body.stmts.push(Stmt::Expr { expr: p, semi: false });
                                StmtId(len32(b.body.stmts.len() - 1))
                            })
                        }
                        _ => None,
                    };
                    if let Some(st) = stmt {
                        if let Some(p) = tail.take().filter(|_| matches!(k, Val::Stmt(_))) {
                            b.body.stmts.push(Stmt::Expr { expr: p, semi: false });
                            stmts.push(StmtId(len32(b.body.stmts.len() - 1)));
                        }
                        stmts.push(st);
                    }
                }
                let list = list_of(&mut b.body.stmt_lists, stmts.iter().copied());
                let scope = b.scopes.pop().unwrap_or(ScopeId(0));
                let mark = b.marks.pop().unwrap_or(0);
                b.bindings.truncate(mark);
                Val::Expr(self.expr(Expr::Block { stmts: list, tail, scope }, s, e, 0)?)
            }
            BLOCK_EXPR => {
                if has(CONST_KW) || has(ASYNC_KW) || has(TRY_KW) || has(GEN_KW) {
                    return Err(self.unsupported(s, "a const, async, try or gen block"));
                }
                let Some(block) = exprs().last() else { return Err(syntax) };
                if parent == Some(FN) {
                    self.b(s)?.in_body = false;
                }
                Val::Expr(block)
            }
            EXPR_STMT => {
                let Some(expr) = exprs().next() else { return Err(syntax) };
                let b = self.b(s)?;
                b.body.stmts.push(Stmt::Expr { expr, semi: has(SEMICOLON) });
                Val::Stmt(StmtId(len32(b.body.stmts.len() - 1)))
            }
            LET_STMT => {
                if has(ELSE_KW) {
                    return Err(self.unsupported(s, "`let .. else`"));
                }
                let Some(pat) = kids.iter().find_map(|k| if let Val::Pat(p) = k { Some(*p) } else { None }) else {
                    return Err(syntax);
                };
                let ty = kids.iter().find_map(|k| if let Val::Ty(t) = k { Some(*t) } else { None });
                let init = exprs().next();
                let b = self.b(s)?;
                b.body.stmts.push(Stmt::Let { pat, ty, init, else_: None });
                let moved = b.pending.split_off(frame.mark);
                b.bindings.extend(moved);
                Val::Stmt(StmtId(len32(b.body.stmts.len() - 1)))
            }
            PARAM => {
                let (Some(Val::Pat(pat)), Some(Val::Ty(ty))) = (kids.first(), kids.last()) else {
                    return Err(self.unsupported(s, "this parameter"));
                };
                let b = self.b(s)?;
                let moved = b.pending.split_off(frame.mark);
                b.bindings.extend(moved);
                Val::Param(*pat, *ty)
            }
            IF_EXPR => {
                let mut it = exprs();
                let (Some(cond), Some(then)) = (it.next(), it.next()) else { return Err(syntax) };
                let else_ = it.next();
                Val::Expr(self.expr(Expr::If { cond, then, else_ }, s, e, 0)?)
            }
            WHILE_EXPR | LOOP_EXPR => {
                let id = ExprId(len32(frame.mark));
                let r = self.rel(s, e);
                let b = self.b(s)?;
                b.loops.pop();
                let body = if frame.kind == WHILE_EXPR {
                    let mut it = exprs();
                    let (Some(cond), Some(body)) = (it.next(), it.next()) else { return Err(syntax) };
                    let brk = self.expr(Expr::Break { value: None, target: id }, s, e, 0)?;
                    self.expr(Expr::If { cond, then: body, else_: Some(brk) }, s, e, 0)?
                } else {
                    exprs().next().ok_or(syntax)?
                };
                let b = self.b(s)?;
                b.body.exprs[id.ix()] = Expr::Loop { body };
                b.body.expr_ranges[id.ix()] = r;
                Val::Expr(id)
            }
            BREAK_EXPR | CONTINUE_EXPR => {
                let Some(&target) = self.b(s)?.loops.last() else {
                    return Err(self.unsupported(s, "`break` or `continue` outside a loop"));
                };
                let x = if frame.kind == BREAK_EXPR {
                    Expr::Break { value: exprs().next(), target }
                } else {
                    Expr::Continue { target }
                };
                Val::Expr(self.expr(x, s, e, 0)?)
            }
            RETURN_EXPR => Val::Expr(self.expr(Expr::Return { value: exprs().next() }, s, e, 0)?),
            MATCH_GUARD => Val::Guard(exprs().next().ok_or(syntax)?),
            MATCH_ARM => {
                let pat = kids.iter().find_map(|k| if let Val::Pat(p) = k { Some(*p) } else { None });
                let guard = kids.iter().find_map(|k| if let Val::Guard(g) = k { Some(*g) } else { None });
                let (Some(pat), Some(body)) = (pat, exprs().next()) else { return Err(syntax) };
                let b = self.b(s)?;
                let mark = b.marks.pop().unwrap_or(0);
                b.bindings.truncate(mark);
                Val::Arm(Arm { pat, guard, body })
            }
            MATCH_EXPR => {
                let Some(scrutinee) = exprs().next() else { return Err(syntax) };
                let b = self.b(s)?;
                let arms =
                    list_of(&mut b.body.arms, kids.iter().filter_map(|k| if let Val::Arm(a) = k { Some(*a) } else { None }));
                Val::Expr(self.expr(Expr::Match { scrutinee, arms }, s, e, 0)?)
            }

            // Items.
            RECORD_FIELD => {
                let (a, b) = self.name_in(kids, s)?;
                let Some(&Val::Ty(t)) = kids.last() else { return Err(syntax) };
                Val::Field(a, b, t)
            }
            STRUCT => {
                self.item_attrs(kids, &[])?;
                if has(SEMICOLON) {
                    return Err(self.unsupported(s, "a unit struct"));
                }
                let (a, b) = self.name_in(kids, s)?;
                let name = self.text(a, b);
                let path = self.module_path(name);
                let m = &self.program.modules[self.module as usize].path;
                let key = item_key(m, b's', name);
                let ctor = item_key(m, b'c', name);
                let mut fields = Vec::new_in(Global);
                for k in kids {
                    if let Val::Field(fa, fb, ty) = *k {
                        fields.push(Field { name: name_of(self.text(fa, fb)), ty });
                    }
                }
                let mut variants = Vec::new_in(Global);
                variants.push(Variant {
                    key: ctor,
                    name: name_of(name),
                    ctor: CtorKind::Braces,
                    fields: List { start: 0, len: len32(fields.len()) },
                });
                let i = len32(self.program.structs.len());
                self.register(name, ItemRef::Struct(i), Some(key), s)?;
                self.program.structs.push(StructItem { key, path, file: self.file, at: s, variants, fields });
                return Ok(None);
            }
            TRAIT => {
                self.item_attrs(kids, &[AttrKind::Lang(LangItem::Sized)])?;
                if has(UNSAFE_KW) || has(AUTO_KW) || has(COLON) {
                    return Err(self.unsupported(s, "an unsafe or auto trait, or supertraits"));
                }
                let (a, b) = self.name_in(kids, s)?;
                let name = self.text(a, b);
                let key = item_key(&self.program.modules[self.module as usize].path, b't', name);
                let i = len32(self.program.traits.len());
                self.register(name, ItemRef::Trait(i), Some(key), s)?;
                self.program.traits.push(TraitItem { key, path: self.module_path(name) });
                for k in kids {
                    if let Val::Attr(Attr { kind: AttrKind::Lang(l), .. }) = k {
                        self.program.lang.push((*l, key));
                    }
                }
                return Ok(None);
            }
            MODULE => {
                self.item_attrs(kids, &[])?;
                let (a, b) = self.name_in(kids, s)?;
                let name = self.text(a, b);
                let file = self.module_file(name, s)?;
                let i = len32(self.program.modules.len());
                self.register(name, ItemRef::Mod(i), None, s)?;
                let path = self.module_path(name);
                self.program.modules.push(Module { path, parent: Some(self.module), file });
                return Ok(None);
            }
            EXTERN_BLOCK => {
                self.item_attrs(kids, &[])?;
                self.extern_abi = None;
                return Ok(None);
            }
            FN => {
                self.finish_fn(parent, kids, s)?;
                return Ok(None);
            }
            SOURCE_FILE => {
                for k in kids {
                    let Val::Attr(a) = *k else { continue };
                    let root_only = |w: &Self| {
                        if w.is_root { Ok(()) } else { Err(w.unsupported(a.at, "this attribute outside the crate root")) }
                    };
                    match a.kind {
                        AttrKind::Doc => {}
                        AttrKind::Features => root_only(self)?,
                        AttrKind::NoCore => {
                            root_only(self)?;
                            self.program.no_core = true;
                        }
                        AttrKind::NoMain => {
                            root_only(self)?;
                            self.program.no_main = true;
                        }
                        _ => return Err(self.unsupported(a.at, "this attribute on a file")),
                    }
                }
                return Ok(None);
            }
            _ => return Err(LowerError::UnsupportedSyntax { file: self.file, at: s, kind: frame.kind }),
        }))
    }

    /// The file of `mod name;` in this module's file.
    fn module_file(&self, name: &str, at: u32) -> Result<u32, LowerError> {
        let here = self.files[self.file as usize].path;
        let (dir, stem) = match here.rfind('/') {
            Some(i) => (&here[..i], &here[i + 1..]),
            None => ("", here),
        };
        let owns_dir = self.is_root || stem == "mod.rs";
        let mut base = std::string::String::new();
        base.push_str(dir);
        if !owns_dir {
            if !base.is_empty() {
                base.push('/');
            }
            base.push_str(stem.strip_suffix(".rs").unwrap_or(stem));
        }
        if !base.is_empty() {
            base.push('/');
        }
        let mut flat = base.clone();
        flat.push_str(name);
        flat.push_str(".rs");
        let mut nested = base;
        nested.push_str(name);
        nested.push_str("/mod.rs");
        let find = |p: &str| self.files.iter().position(|f| f.path == p);
        let file = match (find(&flat), find(&nested)) {
            (Some(f), None) | (None, Some(f)) => f as u32,
            (Some(_), Some(_)) => {
                return Err(LowerError::AmbiguousModuleFile { file: self.file, at, name: boxed(name) });
            }
            (None, None) => return Err(LowerError::NoModuleFile { file: self.file, at, name: boxed(name) }),
        };
        if self.program.modules.iter().any(|m| m.file == file) {
            return Err(LowerError::ModuleFileTwice { file: self.file, at, name: boxed(name) });
        }
        Ok(file)
    }

    fn finish_fn(&mut self, parent: Option<SyntaxKind>, kids: &[Val], s: u32) -> Result<(), LowerError> {
        let has = |kind: SyntaxKind| kids.iter().any(|k| matches!(k, Val::Tok(t, ..) if *t == kind));
        let Some(builder) = self.body.take() else { return Err(LowerError::Syntax { file: self.file, at: s }) };
        let in_extern = parent == Some(EXTERN_ITEM_LIST);
        if has(CONST_KW) || has(ASYNC_KW) || has(GEN_KW) || has(DEFAULT_KW) {
            return Err(self.unsupported(s, "a const, async, gen or default fn"));
        }
        let allowed: &[AttrKind] =
            if in_extern { &[AttrKind::LinkName(0, 0)] } else { &[AttrKind::NoMangle] };
        self.item_attrs(kids, allowed)?;
        let (a, b) = self.name_in(kids, s)?;
        let name = self.text(a, b);
        let body = kids.iter().find_map(|k| if let Val::Expr(x) = k { Some(*x) } else { None });
        match (in_extern, body) {
            (true, Some(_)) => return Err(self.unsupported(s, "a fn with a body in an extern block")),
            (false, None) => return Err(self.unsupported(s, "a fn without a body")),
            _ => {}
        }
        let abi = if in_extern {
            self.extern_abi.unwrap_or(Abi::C)
        } else {
            kids.iter().find_map(|k| if let Val::Abi(x) = k { Some(*x) } else { None }).unwrap_or(Abi::Rust)
        };
        let is_unsafe = has(UNSAFE_KW) || (in_extern && !has(SAFE_KW));
        let no_mangle = kids.iter().any(|k| matches!(k, Val::Attr(Attr { kind: AttrKind::NoMangle, .. })));
        let link_name = kids.iter().find_map(|k| match k {
            Val::Attr(Attr { kind: AttrKind::LinkName(x, y), .. }) => Some(boxed(self.text(*x, *y))),
            _ => None,
        });
        let start = len32(self.program.sig_inputs.len());
        let mut params: Vec<PatId, Global> = Vec::new_in(Global);
        for k in kids {
            if let Val::Param(p, t) = *k {
                params.push(p);
                self.program.sig_inputs.push(t);
            }
        }
        let inputs = List { start, len: len32(self.program.sig_inputs.len()) - start };
        let output = match kids.iter().find_map(|k| if let Val::Ty(t) = k { Some(*t) } else { None }) {
            Some(t) => t,
            None => {
                self.program.sig_nodes.push(PNode::Tuple(List::EMPTY));
                PTy(len32(self.program.sig_nodes.len() - 1))
            }
        };
        let index = len32(self.program.fns.len());
        let key = item_key(&self.program.modules[self.module as usize].path, b'f', name);
        self.register(name, ItemRef::Fn(index), Some(key), s)?;
        let body = match body {
            Some(root) => {
                let BodyB { body: mut out, fixups, pending, .. } = builder;
                if !pending.is_empty() {
                    return Err(self.unsupported(s, "a binding outside a `let`, a parameter or an arm"));
                }
                out.root = root;
                out.params = params;
                for mut fix in fixups {
                    fix.table = Some(index);
                    self.fixups.push(fix);
                }
                Some(out)
            }
            None => None,
        };
        let path = self.module_path(name);
        self.program.fns.push(FnItem {
            key,
            path,
            abi,
            no_mangle,
            is_unsafe,
            body,
            file: self.file,
            at: s,
            module: self.module,
            link_name,
            name: boxed(name),
            inputs,
            output,
        });
        Ok(())
    }
}
