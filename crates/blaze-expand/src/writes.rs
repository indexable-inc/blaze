//! WHAT A MACRO CALL CAN WRITE (decision D134, names on demand; lane LZ2, 2026-11-18): a sound
//! over-approximation of the names an item-position call of a definition can bind in the module it
//! is written in, read from the definition alone, so that a name resolver that asks one name
//! expands only the calls whose summary allows it (blaze-names-demand's soundness rule: a module is
//! known for a name when every unexpanded call in it is summarised as unable to write the name).
//!
//! # The rule (in doubt the answer is `Any`)
//!
//! Every arm's transcriber is walked as a sequence of items (a repetition's body read once in
//! place: a body that writes a name writes it whether it repeats once or ten times):
//!
//! - An item keyword (`fn`, `struct`, `enum`, `union`, `trait`, `type`, `const`, `static`, `mod`,
//!   `macro_rules!`, `macro`, `extern crate`) followed by a LITERAL identifier writes that name.
//!   Followed by a metavariable, the name is an identifier of the call's input ([`MacroWrites::from_input`]):
//!   the definition alone cannot say which, so the definition's own summary is `Any`
//!   ([`AnyWhy::InputNames`]), and a call's summary ([`MacroWrites::for_call`]) lists the input's
//!   identifiers. That refinement is sound because `macro_rules!` transcription only copies tokens:
//!   every identifier of an expansion is one of the transcriber's or one of the input's (`${concat}`
//!   makes new ones: [`AnyWhy::Concat`]).
//! - `use`: each name its tree binds (`as` renames, `self` in braces binds its parent segment); a
//!   glob is [`AnyWhy::Glob`]; a `tt` or other fragment in the tree is [`AnyWhy::UseFromInput`].
//! - `impl` writes no name of its own; a `fn`'s, `trait`'s or `impl`'s body writes nothing in the
//!   module; an inline `mod m { .. }` writes `m`, and of its body only what leaves it: a
//!   `#[macro_export]` macro, and under `#[macro_use]` its `macro_rules!` names. An `extern { .. }`
//!   block's foreign items bind in the module.
//! - A metavariable where an item starts (`$i:item`, `$s:stmt`, a `tt`, an `ident` that may be a
//!   keyword, an `expr` the parser may read through) is [`AnyWhy::ItemFromInput`]; a `vis` is a
//!   visibility, a `literal` or `lifetime` starts no item.
//! - A nested macro call in item position is resolved through [`Callees`]: `$crate::m!` and, under
//!   `local_inner_macros`, `m!` name the definition's crate's `m`; a bare `m!` is what the caller
//!   says it is (blaze-names-source asks the macro namespace). Its summary is folded in (the nested
//!   call's literal identifiers stand for its input), recursion bounded by [`MOST_NESTED`]; a call
//!   that names no known definition is [`AnyWhy::NestedCall`].
//! - Attributes: the inert builtins (`doc`, `cfg`, `allow`, `inline`, `repr`, `stable`, `rustc_*`,
//!   tool paths, ..) write nothing; `cfg_attr` is read through; `#[derive(..)]` of builtin derives
//!   writes impls only; `#[global_allocator]` writes its four symbols; any other attribute, or one
//!   from a metavariable, may be an attribute macro: [`AnyWhy::AttrMacro`], [`AnyWhy::AttrFromInput`].
//!   `#[cfg]` on a transcribed item is counted as written (over-approximation).
//! - A transcriber that is an expression or statements writes nothing: in item position its tokens
//!   are no item (the walk steps over a token that starts no item, as the parser's recovery does).
//!
//! The summary of a definition is the union over its arms (every style: fn-like, `attr`, `derive`).
//!
//! # Builtins and proc macros
//!
//! - Builtin function-like macros write no name in item position, but `include!` (a file's items)
//!   and `cfg_select!` (the selected tokens): `Any`.
//! - Builtin derives (`Copy`, `Clone`, `Default`, `Debug`, `Hash`, `Ord`, `PartialOrd`, `Eq`,
//!   `PartialEq`, `ConstParamTy`) write impls only: no name.
//! - `#[derive]`/`#[derive_const]` as an attribute keeps its item (its name is the input's) and
//!   calls each derive on its own. `#[global_allocator]` writes `__rust_alloc`, `__rust_dealloc`,
//!   `__rust_realloc`, `__rust_alloc_zeroed` (crate::global_allocator) and keeps its static.
//!   `#[test]`, `#[bench]`, `#[test_case]`, `#[cfg_eval]`, `#[alloc_error_handler]`: `Any`.
//! - A proc macro of any kind: `Any`.

use blaze_own::{AVec, Alloc, Vec};

use crate::def::{BuiltinAttr, BuiltinDerive, BuiltinFn, LocalInner, MacroContent, MacroDefinition, MacroKind};
use crate::error::ExpandError;
use crate::rules::{self, Def, Frag, Loc, NONE, Op, Program, Sep};
use crate::tokens::{Delim, Raw, TokenKind, TokenStream};

/// How deep nested calls are folded (a macro calling a macro calling ..); past it, `Any`.
pub const MOST_NESTED: usize = 8;
/// How deep repetitions and groups nest in the walk; past it, `Any`.
pub const MOST_WALK_DEPTH: usize = 64;

/// Why a summary is `Any`: the first cause met.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnyWhy {
    /// A `use` glob.
    Glob,
    /// A metavariable where an item starts.
    ItemFromInput,
    /// A fragment in a `use` tree that may be a glob or a group.
    UseFromInput,
    /// An attribute from a metavariable: it may be an attribute macro.
    AttrFromInput,
    /// An attribute that is no inert builtin: it may be an attribute macro.
    AttrMacro,
    /// A derive that is no builtin derive (a proc macro's).
    Derive,
    /// A nested call that names no definition known here.
    NestedCall,
    /// `${concat(..)}`: an identifier made by the expansion.
    Concat,
    /// `$x` that the arm's matcher does not bind.
    UnknownVar,
    /// A repetition separated by an identifier or a literal (it may make an item keyword).
    Separator,
    /// Nesting past [`MOST_NESTED`] or [`MOST_WALK_DEPTH`].
    Depth,
    /// `#[macro_use] extern crate`: every exported macro of a crate.
    MacroUseExtern,
    /// Names are identifiers of a call's input, and no input was given (a definition's summary).
    InputNames,
    BuiltinFn(BuiltinFn),
    BuiltinAttr(BuiltinAttr),
    ProcMacro,
}

/// What wrote a name: the planted control of the soundness test drops one of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    Item,
    Use,
    Macro,
    ExternCrate,
    /// A nested call's input identifier, standing for the names its callee takes from its input.
    Nested,
    /// A builtin's own symbol (`#[global_allocator]`).
    Builtin,
}

/// The namespaces a written name may bind in (over-approximated: a struct is type and value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WrittenNs {
    pub ty: bool,
    pub value: bool,
    pub macros: bool,
}

impl WrittenNs {
    pub const TYPE: WrittenNs = WrittenNs { ty: true, value: false, macros: false };
    pub const VALUE: WrittenNs = WrittenNs { ty: false, value: true, macros: false };
    pub const TYPE_VALUE: WrittenNs = WrittenNs { ty: true, value: true, macros: false };
    pub const MACRO: WrittenNs = WrittenNs { ty: false, value: false, macros: true };
    pub const ALL: WrittenNs = WrittenNs { ty: true, value: true, macros: true };
}

/// One name a definition writes literally.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Written<'d> {
    pub name: &'d str,
    pub ns: WrittenNs,
    pub by: By,
    /// A `#[macro_export]` macro: bound at the crate root.
    pub exported: bool,
}

/// How a nested call names its macro.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Callee<'d> {
    /// `$crate::m!` (or `m!` under `local_inner_macros`): the definition's crate's `m`.
    Crate(&'d str),
    /// `m!`: what `m` names where the expansion is.
    Bare(&'d str),
}

/// The definitions nested calls name: the caller's (a table in a test, the macro namespace in
/// blaze-names-source). `None` when it cannot be known: the call is then `Any`.
pub trait Callees<'d> {
    fn callee(&self, callee: Callee<'d>) -> Option<&'d MacroDefinition<'d>>;
}

/// A nested call not yet folded.
#[derive(Debug)]
pub struct Nested<'d, 'a> {
    pub callee: Callee<'d>,
    /// The identifiers its input holds literally (keywords too: an over-approximation).
    pub literals: AVec<'a, &'d str>,
    /// Its input holds a metavariable: the outer call's input reaches the callee.
    pub has_vars: bool,
    /// Its input holds `${concat(..)}`.
    pub made: bool,
}

/// A call's summary ([`MacroWrites::for_call`]): the names are in the caller's list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallWrites {
    Any(AnyWhy),
    /// Only the listed names; `exports`: it may write a `#[macro_export]` macro of one of them.
    Only { exports: bool },
}

/// What a definition can write ([`MacroWrites::of`], then [`MacroWrites::close`]).
#[derive(Debug)]
pub struct MacroWrites<'d, 'a> {
    content: MacroContent,
    any: Option<AnyWhy>,
    names: AVec<'a, Written<'d>>,
    from_input: bool,
    exports_from_input: bool,
    nested: AVec<'a, Nested<'d, 'a>>,
}

/// The summary of `def` with its nested calls folded through `callees`.
pub fn macro_writes<'d, 'a, C: Callees<'d>>(
    def: &'d MacroDefinition<'d>,
    callees: &C,
    alloc: Alloc<'a>,
) -> Result<MacroWrites<'d, 'a>, ExpandError> {
    let mut writes = MacroWrites::of(def, alloc)?;
    writes.close(callees, alloc)?;
    Ok(writes)
}

impl<'d, 'a> MacroWrites<'d, 'a> {
    fn empty(content: MacroContent, alloc: Alloc<'a>) -> MacroWrites<'d, 'a> {
        MacroWrites {
            content,
            any: None,
            names: Vec::new_in(alloc),
            from_input: false,
            exports_from_input: false,
            nested: Vec::new_in(alloc),
        }
    }

    /// The summary of `def` read from it alone: its nested calls listed, not folded.
    pub fn of(def: &'d MacroDefinition<'d>, alloc: Alloc<'a>) -> Result<MacroWrites<'d, 'a>, ExpandError> {
        let mut out = MacroWrites::empty(def.content(), alloc);
        match def.kind() {
            MacroKind::Rules | MacroKind::Macro2 => {
                let program = rules::compile(def)?;
                let local_inner = def.at().local_inner == LocalInner::Yes;
                for arm in program.arms.iter() {
                    let mut frags: AVec<'a, Option<Frag>> = Vec::new_in(alloc);
                    frags.resize(arm.binders as usize, None);
                    let mut at = arm.locs as usize;
                    while let Some(loc) = program.locs.get(at) {
                        match *loc {
                            Loc::Eof => break,
                            Loc::Var { binder, kind, .. } if (binder as usize) < frags.len() => frags[binder as usize] = kind,
                            _ => {}
                        }
                        at += 1;
                    }
                    let walk = Walk { p: &program, d: Def::of(def), frags: &frags, local_inner, alloc };
                    let atoms = walk.atoms(arm.ops, arm.ops_end, 0, &mut out);
                    walk.items(&atoms, 0, &mut out);
                }
            }
            MacroKind::BuiltinFn(which) => match which {
                BuiltinFn::Include | BuiltinFn::CfgSelect => out.set_any(AnyWhy::BuiltinFn(which)),
                _ => {}
            },
            MacroKind::BuiltinAttr(which) => match which {
                BuiltinAttr::Derive | BuiltinAttr::DeriveConst => out.from_input = true,
                BuiltinAttr::GlobalAllocator => {
                    out.from_input = true;
                    for name in ALLOCATOR_SYMBOLS {
                        out.names.push(Written { name, ns: WrittenNs::VALUE, by: By::Builtin, exported: false });
                    }
                }
                BuiltinAttr::Test
                | BuiltinAttr::Bench
                | BuiltinAttr::TestCase
                | BuiltinAttr::CfgEval
                | BuiltinAttr::AllocErrorHandler => out.set_any(AnyWhy::BuiltinAttr(which)),
            },
            MacroKind::BuiltinDerive(_) => {}
            MacroKind::ProcMacro { .. } => out.set_any(AnyWhy::ProcMacro),
        }
        Ok(out)
    }

    /// Folds every nested call through `callees`, bounded by [`MOST_NESTED`].
    pub fn close<C: Callees<'d>>(&mut self, callees: &C, alloc: Alloc<'a>) -> Result<(), ExpandError> {
        self.close_at(callees, 0, alloc)
    }

    fn close_at<C: Callees<'d>>(&mut self, callees: &C, depth: usize, alloc: Alloc<'a>) -> Result<(), ExpandError> {
        let nested = core::mem::replace(&mut self.nested, Vec::new_in(alloc));
        let mut selves: AVec<'a, usize> = Vec::new_in(alloc);
        for (at, call) in nested.iter().enumerate() {
            if call.made {
                self.set_any(AnyWhy::Concat);
            }
            let Some(def) = callees.callee(call.callee) else {
                self.set_any(AnyWhy::NestedCall);
                continue;
            };
            if def.content() == self.content {
                selves.push(at);
                continue;
            }
            if depth + 1 >= MOST_NESTED {
                self.set_any(AnyWhy::Depth);
                continue;
            }
            let mut sub = MacroWrites::of(def, alloc)?;
            sub.close_at(callees, depth + 1, alloc)?;
            if let Some(why) = sub.any {
                self.set_any(why);
            }
            if !sub.nested.is_empty() {
                self.set_any(AnyWhy::NestedCall);
            }
            self.names.extend_from_slice(&sub.names);
            if sub.from_input {
                self.take_input(call, sub.exports_from_input);
            }
        }
        // A call of the definition itself writes its own names (already here) and, when the
        // definition takes names from its input, the call's input's: folded once all others are,
        // so `from_input` is final.
        for at in selves.iter() {
            if self.from_input {
                let exports = self.exports_from_input;
                self.take_input(&nested[*at], exports);
            }
        }
        Ok(())
    }

    /// A callee that takes names from its input, called with `call`'s input: the input's
    /// literal identifiers, and the outer input's when the input holds a metavariable.
    fn take_input(&mut self, call: &Nested<'d, 'a>, exports: bool) {
        for name in call.literals.iter() {
            self.names.push(Written { name: *name, ns: WrittenNs::ALL, by: By::Nested, exported: exports });
        }
        if call.has_vars {
            self.from_input = true;
            self.exports_from_input |= exports;
        }
    }

    fn set_any(&mut self, why: AnyWhy) {
        self.any.get_or_insert(why);
    }

    /// Why it is `Any` regardless of the call, if it is.
    pub fn any(&self) -> Option<AnyWhy> {
        self.any
    }

    /// The names it writes literally, in walk order (duplicates kept).
    pub fn names(&self) -> &[Written<'d>] {
        &self.names
    }

    /// Whether a name it writes may be an identifier of the call's input.
    pub fn from_input(&self) -> bool {
        self.from_input
    }

    /// The nested calls not folded ([`MacroWrites::close`] folds them).
    pub fn open_calls(&self) -> &[Nested<'d, 'a>] {
        &self.nested
    }

    /// The summary of one call whose input is `input` (`None`: the definition's own summary): the
    /// names in `out`, sorted and each once.
    pub fn for_call<'w>(&self, input: Option<&'w TokenStream<'w>>, out: &mut AVec<'_, &'w str>) -> CallWrites
    where
        'd: 'w,
    {
        self.for_call_keeping(input, &|_: &Written<'d>| true, out)
    }

    /// [`MacroWrites::for_call`] with only the literal names `keep` keeps: the seam a planted
    /// control drops one kind of name at (`|w| w.by != By::Use`).
    pub fn for_call_keeping<'w>(
        &self,
        input: Option<&'w TokenStream<'w>>,
        keep: &impl Fn(&Written<'d>) -> bool,
        out: &mut AVec<'_, &'w str>,
    ) -> CallWrites
    where
        'd: 'w,
    {
        out.clear();
        if let Some(why) = self.any {
            return CallWrites::Any(why);
        }
        if !self.nested.is_empty() {
            return CallWrites::Any(AnyWhy::NestedCall);
        }
        let mut exports = false;
        for written in self.names.iter().filter(|w| keep(w)) {
            out.push(written.name);
            exports |= written.exported;
        }
        if self.from_input {
            let Some(input) = input else { return CallWrites::Any(AnyWhy::InputNames) };
            for token in input.tokens() {
                if let TokenKind::Ident { .. } = token.kind {
                    out.push(input.text_of(token));
                }
            }
            exports |= self.exports_from_input;
        }
        out.sort_unstable();
        out.dedup();
        CallWrites::Only { exports }
    }
}

/// `#[global_allocator]`'s symbols (crate::global_allocator).
const ALLOCATOR_SYMBOLS: [&str; 4] = ["__rust_alloc", "__rust_dealloc", "__rust_realloc", "__rust_alloc_zeroed"];

/// The builtin attributes that write nothing (rustc's `BUILTIN_ATTRIBUTES` that are no macro).
const INERT: &[&str] = &[
    "doc", "cfg", "allow", "warn", "deny", "forbid", "expect", "deprecated", "must_use", "inline", "cold",
    "track_caller", "repr", "non_exhaustive", "automatically_derived", "no_mangle", "export_name", "link_name",
    "link_section", "link", "link_ordinal", "used", "path", "stable", "unstable", "allow_internal_unstable",
    "allow_internal_unsafe", "target_feature", "naked", "optimize", "coverage", "instruction_set", "fundamental",
    "lang", "marker", "may_dangle", "ffi_const", "ffi_pure", "thread_local", "linkage", "debugger_visualizer",
    "collapse_debuginfo", "no_implicit_prelude", "macro_escape", "proc_macro", "proc_macro_attribute",
    "proc_macro_derive", "no_link", "panic_handler", "const_trait", "prelude_import", "ignore", "should_panic",
    "type_const", "align", "export_stable", "define_opaque", "unstable_feature_bound", "patchable_function_entry",
    "pointee", "no_builtins", "recursion_limit", "type_length_limit", "feature", "no_std", "no_core",
    "compiler_builtins", "needs_allocator", "panic_runtime", "needs_panic_runtime", "profiler_runtime",
    "windows_subsystem", "crate_type", "crate_name", "default_lib_allocator", "omit_gdb_pretty_printer_section",
    "test_runner", "reexport_test_harness_main", "deny_until",
];

/// Tool namespaces whose attributes are inert (`#[rustfmt::skip]`, `#[diagnostic::..]`).
const TOOLS: &[&str] = &["rustfmt", "clippy", "diagnostic", "rustc", "rustdoc", "miri", "rust_analyzer"];

/// One token or token tree of a transcriber, a repetition's body read once in place.
#[derive(Clone, Copy, Debug)]
enum Atom {
    Word { tok: rules::DefTok, raw: bool },
    Lit,
    /// Punctuation glued into one operator: its first character and length.
    Punct { ch: char, len: u8 },
    Var { binder: u32 },
    Crate,
    /// A group; its body is the template operations `from..to`.
    Group { delim: Delim, from: u32, to: u32 },
    /// `${concat(..)}`.
    Made,
}

/// A segment of a `use` path, as far as the name it binds.
#[derive(Clone, Copy)]
enum Seg<'d> {
    Lit(&'d str),
    Input,
    SelfKw,
    /// `crate`, `super`, `$crate`: binds nothing alone.
    Kw,
}

#[derive(Clone, Copy, Default)]
struct AttrFlags {
    exported: bool,
    macro_use: bool,
}

struct Walk<'p, 'd, 'a> {
    p: &'p Program<'d>,
    d: Def<'d>,
    /// Each binder's fragment (`None`: a specifier no edition has, read as a `tt`).
    frags: &'p [Option<Frag>],
    local_inner: bool,
    alloc: Alloc<'a>,
}

impl<'p, 'd, 'a> Walk<'p, 'd, 'a> {
    /// The atoms of the template operations `from..to`.
    fn atoms(&self, from: u32, to: u32, depth: usize, out: &mut MacroWrites<'d, 'a>) -> AVec<'a, Atom> {
        let mut atoms = Vec::new_in(self.alloc);
        self.atoms_into(from, to, depth, out, &mut atoms);
        atoms
    }

    fn atoms_into(&self, from: u32, to: u32, depth: usize, out: &mut MacroWrites<'d, 'a>, atoms: &mut AVec<'a, Atom>) {
        let mut stack: AVec<'a, (usize, usize, usize)> = AVec::new_in(self.alloc);
        stack.push((from as usize, to as usize, depth));
        while let Some((at, to, d)) = stack.last_mut() {
            if *d > MOST_WALK_DEPTH {
                out.set_any(AnyWhy::Depth);
                stack.pop();
                continue;
            }
            if *at >= *to {
                stack.pop();
                continue;
            }
            let cur = *at;
            match self.p.ops[cur] {
                Op::Tok(tok) => {
                    atoms.push(match self.d.kind(tok) {
                        TokenKind::Ident { raw, .. } => Atom::Word { tok, raw: raw == Raw::Yes },
                        _ => Atom::Lit,
                    });
                    *at = cur + 1;
                }
                Op::Crate(_) => {
                    atoms.push(Atom::Crate);
                    *at = cur + 1;
                }
                Op::Puncts { at: tok, len } => {
                    atoms.push(Atom::Punct { ch: self.d.char(tok), len });
                    *at = cur + 1;
                }
                Op::Var { binder, .. } => {
                    atoms.push(Atom::Var { binder });
                    *at = cur + 1;
                }
                Op::Group { delim, end, .. } => {
                    atoms.push(Atom::Group { delim, from: (cur + 1) as u32, to: end });
                    *at = end as usize;
                }
                Op::Repeat { end, sep, .. } => {
                    if sep != NONE && matches!(self.p.seps.get(sep as usize), Some(Sep::Ident(_) | Sep::Literal(_))) {
                        out.set_any(AnyWhy::Separator);
                    }
                    let next_d = *d + 1;
                    *at = end as usize;
                    stack.push((cur + 1, end as usize, next_d));
                }
                Op::Ignore { .. } => {
                    *at = cur + 1;
                }
                Op::Index { .. } | Op::Len { .. } | Op::Count { .. } => {
                    atoms.push(Atom::Lit);
                    *at = cur + 1;
                }
                Op::Concat { .. } => {
                    atoms.push(Atom::Made);
                    *at = cur + 1;
                }
            }
        }
    }

    fn frag(&self, binder: u32) -> Result<Option<Frag>, ()> {
        match self.frags.get(binder as usize) {
            Some(frag) => Ok(*frag),
            None => Err(()),
        }
    }

    fn word(&self, atom: Option<&Atom>) -> Option<&'d str> {
        match atom {
            Some(Atom::Word { tok, raw: false }) => Some(self.d.text(*tok)),
            _ => None,
        }
    }

    /// Every item of `atoms`, one after another.
    fn items(&self, atoms: &[Atom], depth: usize, out: &mut MacroWrites<'d, 'a>) {
        let mut at = 0;
        while at < atoms.len() {
            at = self.item(atoms, at, depth, out).max(at + 1);
        }
    }

    /// One item from `start`; gives where the next starts.
    fn item(&self, atoms: &[Atom], start: usize, depth: usize, out: &mut MacroWrites<'d, 'a>) -> usize {
        let mut at = start;
        let mut flags = AttrFlags::default();
        while let Some(Atom::Punct { ch: '#', len }) = atoms.get(at) {
            let mut next = at + 1;
            if *len == 1 && matches!(atoms.get(next), Some(Atom::Punct { ch: '!', len: 1 })) {
                next += 1;
            }
            let Some(Atom::Group { delim: Delim::Bracket, from, to }) = atoms.get(next) else { break };
            let inner = self.atoms(*from, *to, depth + 1, out);
            self.attr(&inner, depth + 1, &mut flags, out);
            at = next + 1;
        }
        if self.word(atoms.get(at)) == Some("pub") {
            at += 1;
            if matches!(atoms.get(at), Some(Atom::Group { delim: Delim::Paren, .. })) {
                at += 1;
            }
        } else if let Some(Atom::Var { binder }) = atoms.get(at)
            && self.frag(*binder) == Ok(Some(Frag::Vis))
        {
            at += 1;
        }
        if let Some(end) = self.macro_call(atoms, at, out) {
            return end;
        }
        loop {
            let Some(atom) = atoms.get(at) else { return at };
            if let Atom::Var { binder } = *atom {
                match self.frag(binder) {
                    Ok(Some(Frag::Vis | Frag::Lifetime | Frag::Literal)) => {}
                    Ok(_) => out.set_any(AnyWhy::ItemFromInput),
                    Err(()) => out.set_any(AnyWhy::UnknownVar),
                }
                return at + 1;
            }
            let Some(word) = self.word(Some(atom)) else { return at + 1 };
            let next = self.word(atoms.get(at + 1));
            match word {
                "unsafe" | "async" | "default" | "auto" | "safe" | "gen" if next.is_some() => at += 1,
                "const" if matches!(next, Some("fn" | "unsafe" | "async" | "extern")) => at += 1,
                "extern" => match atoms.get(at + 1) {
                    Some(Atom::Word { .. }) if next == Some("crate") => return self.extern_crate(atoms, at + 2, flags, out),
                    Some(Atom::Lit) => match atoms.get(at + 2) {
                        Some(Atom::Group { delim: Delim::Brace, from, to }) => return self.foreign(*from, *to, at + 3, depth, out),
                        _ => at += 2,
                    },
                    Some(Atom::Group { delim: Delim::Brace, from, to }) => return self.foreign(*from, *to, at + 2, depth, out),
                    _ => at += 1,
                },
                "fn" => {
                    self.name(atoms, at + 1, WrittenNs::VALUE, By::Item, false, out);
                    return self.end_block(atoms, at + 2, out);
                }
                "struct" => {
                    self.name(atoms, at + 1, WrittenNs::TYPE_VALUE, By::Item, false, out);
                    return self.end_block(atoms, at + 2, out);
                }
                "enum" | "trait" => {
                    self.name(atoms, at + 1, WrittenNs::TYPE, By::Item, false, out);
                    return self.end_block(atoms, at + 2, out);
                }
                "union" if matches!(atoms.get(at + 1), Some(Atom::Word { .. } | Atom::Var { .. })) => {
                    self.name(atoms, at + 1, WrittenNs::TYPE, By::Item, false, out);
                    return self.end_block(atoms, at + 2, out);
                }
                "type" => {
                    self.name(atoms, at + 1, WrittenNs::TYPE, By::Item, false, out);
                    return self.end_semi(atoms, at + 2, out);
                }
                "const" | "static" => {
                    let mut name = at + 1;
                    if word == "static" && next == Some("mut") {
                        name += 1;
                    }
                    self.name(atoms, name, WrittenNs::VALUE, By::Item, false, out);
                    return self.end_semi(atoms, name + 1, out);
                }
                "mod" => {
                    self.name(atoms, at + 1, WrittenNs::TYPE, By::Item, false, out);
                    return match atoms.get(at + 2) {
                        Some(Atom::Group { delim: Delim::Brace, from, to }) => {
                            self.module(*from, *to, depth, flags, out);
                            at + 3
                        }
                        _ => self.end_semi(atoms, at + 2, out),
                    };
                }
                "use" => {
                    let end = self.semi(atoms, at + 1);
                    self.use_tree(&atoms[at + 1..end], None, depth, out);
                    return end + 1;
                }
                "impl" => return self.end_block(atoms, at + 1, out),
                "macro_rules" if matches!(atoms.get(at + 1), Some(Atom::Punct { ch: '!', len: 1 })) => {
                    self.name(atoms, at + 2, WrittenNs::MACRO, By::Macro, flags.exported, out);
                    let mut end = at + 3;
                    if matches!(atoms.get(end), Some(Atom::Group { .. })) {
                        end += 1;
                    }
                    if matches!(atoms.get(end), Some(Atom::Punct { ch: ';', .. })) {
                        end += 1;
                    }
                    return end;
                }
                "macro" => {
                    self.name(atoms, at + 1, WrittenNs::MACRO, By::Macro, false, out);
                    let mut end = at + 2;
                    if matches!(atoms.get(end), Some(Atom::Group { delim: Delim::Paren, .. })) {
                        end += 1;
                    }
                    if matches!(atoms.get(end), Some(Atom::Group { .. })) {
                        end += 1;
                    }
                    return end;
                }
                _ => return at + 1,
            }
        }
    }

    /// The name at `at` after an item keyword.
    fn name(&self, atoms: &[Atom], at: usize, ns: WrittenNs, by: By, exported: bool, out: &mut MacroWrites<'d, 'a>) {
        match atoms.get(at) {
            Some(Atom::Word { tok, .. }) => {
                let name = self.d.text(*tok);
                if name != "_" {
                    out.names.push(Written { name, ns, by, exported });
                }
            }
            Some(Atom::Var { binder }) => match self.frag(*binder) {
                Ok(_) => {
                    out.from_input = true;
                    out.exports_from_input |= exported;
                }
                Err(()) => out.set_any(AnyWhy::UnknownVar),
            },
            Some(Atom::Made) => out.set_any(AnyWhy::Concat),
            _ => {}
        }
    }

    /// The first `;` from `at` (the atoms' end if none).
    fn semi(&self, atoms: &[Atom], at: usize) -> usize {
        let mut end = at;
        while end < atoms.len() && !matches!(atoms[end], Atom::Punct { ch: ';', .. }) {
            end += 1;
        }
        end
    }

    /// A metavariable in an item's header that may itself be an item.
    fn header(&self, atoms: &[Atom], from: usize, to: usize, out: &mut MacroWrites<'d, 'a>) {
        for atom in atoms.get(from..to).unwrap_or(&[]) {
            if let Atom::Var { binder } = *atom
                && matches!(self.frag(binder), Ok(Some(Frag::Item | Frag::Stmt)))
            {
                out.set_any(AnyWhy::ItemFromInput);
            }
        }
    }

    /// After a `const`, `static` or `type`: past the `;`.
    fn end_semi(&self, atoms: &[Atom], at: usize, out: &mut MacroWrites<'d, 'a>) -> usize {
        let end = self.semi(atoms, at);
        self.header(atoms, at, end, out);
        end + 1
    }

    /// After a `fn`, `struct`, `enum`, `union`, `trait`, `impl` or `mod` header: past its `;` or
    /// its body, a brace group that is no const argument (`<{N}>`, `= {N}`, `, {N}` in angles).
    fn end_block(&self, atoms: &[Atom], at: usize, out: &mut MacroWrites<'d, 'a>) -> usize {
        let mut angles: i32 = 0;
        let mut end = at;
        while let Some(atom) = atoms.get(end) {
            match *atom {
                Atom::Punct { ch: ';', .. } => break,
                // astlog-ignore: expand-hand-extent (LEFT: a macro_rules transcriber's atoms, `$x` among them, which no parser entry reads; the names-demand over-approximation, D158)
                Atom::Punct { ch: '<', len } => angles += i32::from(len.min(2)),
                // astlog-ignore: expand-hand-extent (as above)
                Atom::Punct { ch: '>', len } => angles = (angles - i32::from(len.min(2))).max(0),
                Atom::Group { delim: Delim::Brace, .. } => {
                    let argument = match end.checked_sub(1).and_then(|prev| atoms.get(prev)) {
                        // astlog-ignore: expand-hand-extent (as above)
                        Some(Atom::Punct { ch: '<' | '=', len: 1 }) => true,
                        // astlog-ignore: expand-hand-extent (as above)
                        Some(Atom::Punct { ch: ',', .. }) => angles > 0,
                        _ => false,
                    };
                    if !argument {
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        self.header(atoms, at, end, out);
        end + 1
    }

    /// A macro call at `at` (a path, `!`, a group, an optional `;`): gives where it ends.
    fn macro_call(&self, atoms: &[Atom], at: usize, out: &mut MacroWrites<'d, 'a>) -> Option<usize> {
        let mut end = at;
        let mut segs: [Option<Atom>; 2] = [None, None];
        let mut count = 0usize;
        let mut global = false;
        if matches!(atoms.get(end), Some(Atom::Punct { ch: ':', len: 2 })) {
            global = true;
            end += 1;
        }
        loop {
            match atoms.get(end) {
                Some(atom @ (Atom::Word { .. } | Atom::Crate | Atom::Var { .. })) => {
                    if count < 2 {
                        segs[count] = Some(*atom);
                    }
                    count += 1;
                }
                _ => return None,
            }
            end += 1;
            if matches!(atoms.get(end), Some(Atom::Punct { ch: ':', len: 2 })) {
                end += 1;
                continue;
            }
            break;
        }
        if !matches!(atoms.get(end), Some(Atom::Punct { ch: '!', len: 1 })) {
            return None;
        }
        if count == 1 && self.word(segs[0].as_ref()) == Some("macro_rules") {
            return None;
        }
        let Some(Atom::Group { from, to, .. }) = atoms.get(end + 1) else { return None };
        let (from, to) = (*from, *to);
        end += 2;
        if matches!(atoms.get(end), Some(Atom::Punct { ch: ';', .. })) {
            end += 1;
        }
        let callee = match (global, count, segs) {
            (false, 1, [Some(Atom::Word { tok, .. }), _]) => {
                let name = self.d.text(tok);
                Some(if self.local_inner { Callee::Crate(name) } else { Callee::Bare(name) })
            }
            (false, 2, [Some(Atom::Crate), Some(Atom::Word { tok, .. })]) => Some(Callee::Crate(self.d.text(tok))),
            _ => None,
        };
        let Some(callee) = callee else {
            out.set_any(AnyWhy::NestedCall);
            return Some(end);
        };
        let mut call = Nested { callee, literals: Vec::new_in(self.alloc), has_vars: false, made: false };
        for op in &self.p.ops[from as usize..to as usize] {
            match *op {
                Op::Tok(tok) if matches!(self.d.kind(tok), TokenKind::Ident { .. }) => call.literals.push(self.d.text(tok)),
                Op::Var { .. } | Op::Ignore { .. } => call.has_vars = true,
                Op::Concat { .. } => call.made = true,
                _ => {}
            }
        }
        out.nested.push(call);
        Some(end)
    }

    /// One attribute's inside.
    fn attr(&self, atoms: &[Atom], depth: usize, flags: &mut AttrFlags, out: &mut MacroWrites<'d, 'a>) {
        let mut groups: AVec<'a, AVec<'a, Atom>> = AVec::new_in(self.alloc);
        let mut stack: AVec<'a, (Option<usize>, usize, usize, usize)> = AVec::new_in(self.alloc);
        stack.push((None, 0, atoms.len(), depth));
        while let Some((src, start, end, depth)) = stack.pop() {
            let cur = match src {
                None => atoms.get(start..end).unwrap_or(&[]),
                Some(idx) => groups.get(idx).and_then(|g| g.get(start..end)).unwrap_or(&[]),
            };
            let first = match cur.first() {
                Some(Atom::Var { .. } | Atom::Made) => {
                    out.set_any(AnyWhy::AttrFromInput);
                    continue;
                }
                Some(Atom::Word { tok, .. }) => self.d.text(*tok),
                _ => continue,
            };
            if matches!(cur.get(1), Some(Atom::Punct { ch: ':', len: 2 })) {
                if !TOOLS.contains(&first) {
                    out.set_any(AnyWhy::AttrMacro);
                }
                continue;
            }
            let group = match cur.get(1) {
                Some(Atom::Group { from, to, .. }) => Some(self.atoms(*from, *to, depth + 1, out)),
                _ => None,
            };
            match first {
                "macro_export" => flags.exported = true,
                "macro_use" => flags.macro_use = true,
                "derive" | "derive_const" => {
                    // astlog-ignore: expand-hand-extent (a derive's paths in a transcriber's atoms; a derive path holds no `,`)
                    for part in group.as_deref().unwrap_or(&[]).split(|a| matches!(a, Atom::Punct { ch: ',', .. })) {
                        if part.iter().any(|a| matches!(a, Atom::Var { .. } | Atom::Made)) {
                            out.set_any(AnyWhy::AttrFromInput);
                        } else if let Some(last) = part.iter().rev().find_map(|a| self.word(Some(a)))
                            && !BuiltinDerive::ALL.iter().any(|d| d.name() == last)
                        {
                            out.set_any(AnyWhy::Derive);
                        }
                    }
                }
                "cfg_attr" => {
                    if let Some(g) = group {
                        let idx = groups.len();
                        let mut ranges: AVec<'a, (usize, usize)> = AVec::new_in(self.alloc);
                        let mut part_start = 0usize;
                        let mut skipped_first = false;
                        // astlog-ignore: expand-hand-extent (LEFT: cfg_attr's metas in a transcriber's atoms, `$x` among them, by commas; over-approximates names demand, D158)
                        for (i, a) in g.iter().enumerate() {
                            if matches!(a, Atom::Punct { ch: ',', .. }) {
                                if skipped_first && part_start < i {
                                    ranges.push((part_start, i));
                                }
                                skipped_first = true;
                                part_start = i + 1;
                            }
                        }
                        if skipped_first && part_start < g.len() {
                            ranges.push((part_start, g.len()));
                        }
                        groups.push(g);
                        for &(s, e) in ranges.iter().rev() {
                            stack.push((Some(idx), s, e, depth + 1));
                        }
                    }
                }
                "unsafe" => {
                    if let Some(g) = group {
                        let idx = groups.len();
                        let len = g.len();
                        groups.push(g);
                        stack.push((Some(idx), 0, len, depth + 1));
                    }
                }
                "global_allocator" => {
                    for name in ALLOCATOR_SYMBOLS {
                        out.names.push(Written { name, ns: WrittenNs::VALUE, by: By::Builtin, exported: false });
                    }
                }
                name if INERT.contains(&name) || name.starts_with("rustc_") => {}
                _ => out.set_any(AnyWhy::AttrMacro),
            }
        }
    }

    /// `extern crate name [as rename];` from the name at `at`.
    fn extern_crate(&self, atoms: &[Atom], at: usize, flags: AttrFlags, out: &mut MacroWrites<'d, 'a>) -> usize {
        if flags.macro_use {
            out.set_any(AnyWhy::MacroUseExtern);
        }
        let name = match self.word(atoms.get(at + 1)) {
            Some("as") => at + 2,
            _ => at,
        };
        if self.word(atoms.get(name)) != Some("self") {
            self.name(atoms, name, WrittenNs::TYPE, By::ExternCrate, false, out);
        }
        self.end_semi(atoms, at + 1, out)
    }

    /// An `extern { .. }` block: its foreign items bind in the module.
    fn foreign(&self, from: u32, to: u32, end: usize, depth: usize, out: &mut MacroWrites<'d, 'a>) -> usize {
        let inner = self.atoms(from, to, depth + 1, out);
        self.items(&inner, depth + 1, out);
        end
    }

    /// An inline `mod m { .. }`: what leaves its body.
    fn module(&self, from: u32, to: u32, depth: usize, flags: AttrFlags, out: &mut MacroWrites<'d, 'a>) {
        let mut sub = MacroWrites::empty(out.content, self.alloc);
        let inner = self.atoms(from, to, depth + 1, &mut sub);
        self.items(&inner, depth + 1, &mut sub);
        for written in sub.names.iter() {
            if written.exported || (flags.macro_use && written.by == By::Macro) {
                out.names.push(*written);
            }
        }
        if let Some(why) = sub.any {
            out.set_any(why);
        }
        if sub.exports_from_input || (flags.macro_use && sub.from_input) {
            out.from_input = true;
            out.exports_from_input |= sub.exports_from_input;
        }
        // A call in the body writes there; folded here, its names over-approximate this module's.
        while let Some(call) = sub.nested.pop() {
            out.nested.push(call);
        }
    }

    /// A `use` tree (no `use`, no `;`); `parent`: the segment before its braces.
    fn use_tree(&self, atoms: &[Atom], parent: Option<Seg<'d>>, depth: usize, out: &mut MacroWrites<'d, 'a>) {
        struct Frame<'d, 'a> {
            owned: Option<AVec<'a, Atom>>,
            at: usize,
            last: Option<Seg<'d>>,
            parent: Option<Seg<'d>>,
            depth: usize,
        }
        let mut stack: AVec<'a, Frame<'d, 'a>> = AVec::new_in(self.alloc);
        stack.push(Frame { owned: None, at: 0, last: None, parent, depth });
        while let Some(top) = stack.last_mut() {
            if top.depth > MOST_WALK_DEPTH {
                out.set_any(AnyWhy::Depth);
                stack.pop();
                continue;
            }
            let slice = top.owned.as_deref().unwrap_or(atoms);
            let Some(&atom) = slice.get(top.at) else {
                let last = top.last;
                let parent = top.parent;
                stack.pop();
                self.bind(last, parent, out);
                continue;
            };
            top.at += 1;
            match atom {
                Atom::Word { tok, raw } => {
                    let text = self.d.text(tok);
                    if !raw && text == "as" {
                        match slice.get(top.at) {
                            Some(Atom::Word { tok, .. }) if self.d.text(*tok) != "_" => {
                                out.names.push(Written { name: self.d.text(*tok), ns: WrittenNs::ALL, by: By::Use, exported: false });
                            }
                            Some(Atom::Var { .. }) => out.from_input = true,
                            Some(Atom::Made) => out.set_any(AnyWhy::Concat),
                            _ => {}
                        }
                        top.at += 1;
                        top.last = None;
                    } else {
                        top.last = Some(match text {
                            "self" if !raw => Seg::SelfKw,
                            "crate" | "super" if !raw => Seg::Kw,
                            _ => Seg::Lit(text),
                        });
                    }
                }
                Atom::Crate => top.last = Some(Seg::Kw),
                Atom::Var { binder } => match self.frag(binder) {
                    Ok(Some(Frag::Ident | Frag::Path)) => top.last = Some(Seg::Input),
                    Ok(_) => out.set_any(AnyWhy::UseFromInput),
                    Err(()) => out.set_any(AnyWhy::UnknownVar),
                },
                Atom::Made => out.set_any(AnyWhy::Concat),
                Atom::Punct { ch: '*', .. } => out.set_any(AnyWhy::Glob),
                // astlog-ignore: expand-hand-extent (a `use` tree's separator in a transcriber's atoms; a use path holds no generics)
                Atom::Punct { ch: ',', .. } => {
                    self.bind(top.last.take(), top.parent, out);
                }
                Atom::Group { delim: Delim::Brace, from, to } => {
                    let child_depth = top.depth + 1;
                    let child_parent = top.last.take();
                    let inner = self.atoms(from, to, child_depth, out);
                    stack.push(Frame { owned: Some(inner), at: 0, last: None, parent: child_parent, depth: child_depth });
                }
                _ => {}
            }
        }
    }

    fn bind(&self, seg: Option<Seg<'d>>, parent: Option<Seg<'d>>, out: &mut MacroWrites<'d, 'a>) {
        let seg = match seg {
            Some(Seg::SelfKw) => parent,
            other => other,
        };
        match seg {
            Some(Seg::Lit(name)) => out.names.push(Written { name, ns: WrittenNs::ALL, by: By::Use, exported: false }),
            Some(Seg::Input) => out.from_input = true,
            Some(Seg::SelfKw | Seg::Kw) | None => {}
        }
    }
}
