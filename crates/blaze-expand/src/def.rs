//! What a macro is: [`MacroDef`], and the hash of its content, [`MacroContent`], which is
//! what an expansion's key names it by (never its item key: two definitions with equal content
//! expand alike, and a renamed or moved one keeps its expansions; rule 21).

use core::alloc::Allocator;

use alloc::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, Portable};
use blaze_ids::Hasher;

use crate::{
    call::CallShape,
    error::{StreamError, Table},
    tokens::{DefSite, Edition, Mark, MarkOf, Sink, Text, TokenStream, Transparency, push_text},
};

/// The builtin fn-like macros, by name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuiltinFn {
    FormatArgs,
    FormatArgsNl,
    ConstFormatArgs,
    Concat,
    Stringify,
    /// `core::panic!` and `std::panic!`: `$crate` tells them apart.
    Panic,
    Unreachable,
    Cfg,
    Env,
    OptionEnv,
    IncludeStr,
    IncludeBytes,
    Line,
    Column,
    File,
    ModulePath,
    CompileError,
    Include,
    Assert,
    ConcatBytes,
    /// Not built yet ([`crate::ExpandError::NotBuilt`]).
    Asm,
    /// Not built yet.
    GlobalAsm,
    /// Not built yet.
    NakedAsm,
    CfgSelect,
    /// Not built yet.
    TraceMacros,
    /// Not built yet.
    LogSyntax,
}

impl BuiltinFn {
    pub fn name(self) -> &'static str {
        match self {
            BuiltinFn::FormatArgs => "format_args",
            BuiltinFn::FormatArgsNl => "format_args_nl",
            BuiltinFn::ConstFormatArgs => "const_format_args",
            BuiltinFn::Concat => "concat",
            BuiltinFn::Stringify => "stringify",
            BuiltinFn::Panic => "panic",
            BuiltinFn::Unreachable => "unreachable",
            BuiltinFn::Cfg => "cfg",
            BuiltinFn::Env => "env",
            BuiltinFn::OptionEnv => "option_env",
            BuiltinFn::IncludeStr => "include_str",
            BuiltinFn::IncludeBytes => "include_bytes",
            BuiltinFn::Line => "line",
            BuiltinFn::Column => "column",
            BuiltinFn::File => "file",
            BuiltinFn::ModulePath => "module_path",
            BuiltinFn::CompileError => "compile_error",
            BuiltinFn::Include => "include",
            BuiltinFn::Assert => "assert",
            BuiltinFn::ConcatBytes => "concat_bytes",
            BuiltinFn::Asm => "asm",
            BuiltinFn::GlobalAsm => "global_asm",
            BuiltinFn::NakedAsm => "naked_asm",
            BuiltinFn::CfgSelect => "cfg_select",
            BuiltinFn::TraceMacros => "trace_macros",
            BuiltinFn::LogSyntax => "log_syntax",
        }
    }
}

/// The builtin attribute macros, by name. None is built yet: each is a
/// [`crate::ExpandError::NotBuilt`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuiltinAttr {
    Test,
    Bench,
    TestCase,
    GlobalAllocator,
    CfgEval,
    Derive,
    DeriveConst,
    AllocErrorHandler,
}

impl BuiltinAttr {
    pub fn name(self) -> &'static str {
        match self {
            BuiltinAttr::Test => "test",
            BuiltinAttr::Bench => "bench",
            BuiltinAttr::TestCase => "test_case",
            BuiltinAttr::GlobalAllocator => "global_allocator",
            BuiltinAttr::CfgEval => "cfg_eval",
            BuiltinAttr::Derive => "derive",
            BuiltinAttr::DeriveConst => "derive_const",
            BuiltinAttr::AllocErrorHandler => "alloc_error_handler",
        }
    }
}

/// The builtin derives built here, each written by `crate::derive`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuiltinDerive {
    Copy,
    Clone,
    Default,
    Debug,
    Hash,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
}

impl BuiltinDerive {
    pub fn name(self) -> &'static str {
        match self {
            BuiltinDerive::Copy => "Copy",
            BuiltinDerive::Clone => "Clone",
            BuiltinDerive::Default => "Default",
            BuiltinDerive::Debug => "Debug",
            BuiltinDerive::Hash => "Hash",
            BuiltinDerive::Ord => "Ord",
            BuiltinDerive::PartialOrd => "PartialOrd",
            BuiltinDerive::Eq => "Eq",
            BuiltinDerive::PartialEq => "PartialEq",
        }
    }
}

/// How a proc macro is called.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProcKind {
    FnLike,
    Attr,
    Derive,
}

/// What kind of macro a definition is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MacroKind {
    /// `macro_rules! m { rules }`: its body is the rules.
    Rules,
    /// `macro m { rules }`, or `macro m(params) { body }` with [`MacroDef::params`].
    Macro2,
    BuiltinFn(BuiltinFn),
    BuiltinAttr(BuiltinAttr),
    BuiltinDerive(BuiltinDerive),
    /// A proc macro: its crate's key and its export name ([`MacroDef::proc_name`]).
    ProcMacro {
        krate: CrateItemKey,
        kind: ProcKind,
        name: Text,
    },
}

impl MacroKind {
    /// The kinds of call the macro takes; `None` for a declarative macro, whose rules say.
    pub(crate) fn shape(self) -> Option<CallShape> {
        match self {
            MacroKind::Rules | MacroKind::Macro2 => None,
            MacroKind::BuiltinFn(_) => Some(CallShape::FnLike),
            MacroKind::BuiltinAttr(_) => Some(CallShape::Attr),
            MacroKind::BuiltinDerive(_) => Some(CallShape::Derive),
            MacroKind::ProcMacro { kind: ProcKind::FnLike, .. } => Some(CallShape::FnLike),
            MacroKind::ProcMacro { kind: ProcKind::Attr, .. } => Some(CallShape::Attr),
            MacroKind::ProcMacro { kind: ProcKind::Derive, .. } => Some(CallShape::Derive),
        }
    }
}

/// `#[macro_export]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Export {
    Local,
    Exported,
}

/// `#[macro_export(local_inner_macros)]`: a single-name macro call the body writes resolves as
/// `$crate::name!`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LocalInner {
    No,
    Yes,
}

/// Where and how a macro is defined, besides its tokens.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DefAt {
    pub site: DefSite,
    pub edition: Edition,
    pub export: Export,
    pub local_inner: LocalInner,
    /// `#[rustc_macro_transparency = ".."]`; `None` takes the kind's own.
    pub transparency: Option<Transparency>,
}

/// The hash of what a definition is: its kind, tokens, edition, flags, helper attributes and
/// transparency. Comments and spaces are not tokens, so they are not in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MacroContent(pub ContentId);

// SAFETY: a content hash.
unsafe impl Portable for MacroContent {}
// SAFETY: a closed enum of names.
unsafe impl Portable for BuiltinFn {}
// SAFETY: a closed enum of names.
unsafe impl Portable for BuiltinAttr {}
// SAFETY: a closed enum of names.
unsafe impl Portable for BuiltinDerive {}
// SAFETY: a closed enum.
unsafe impl Portable for ProcKind {}
// SAFETY: closed enums, a crate key and a range of the definition's own name table.
unsafe impl Portable for MacroKind {}
// SAFETY: a closed enum.
unsafe impl Portable for Export {}
// SAFETY: a closed enum.
unsafe impl Portable for LocalInner {}
// SAFETY: a def site of keys, an edition, flags and a transparency.
unsafe impl Portable for DefAt {}
// SAFETY: every field is `Portable` (streams, UTF-8 bytes, ranges of them, keys, closed
// enums); the allocator is where the bytes are.
unsafe impl<A: Allocator> Portable for MacroDef<A> {}

/// A macro's definition.
#[derive(Clone, Debug)]
pub struct MacroDef<A: Allocator> {
    kind: MacroKind,
    body: TokenStream<A>,
    params: Option<TokenStream<A>>,
    /// The text of a proc macro's name and of the helper attributes.
    names: Vec<u8, A>,
    helpers: Vec<Text, A>,
    at: DefAt,
    transparency: Transparency,
    content: MacroContent,
}

impl<A: Allocator + Clone> MacroDef<A> {
    /// `macro_rules! m { body }`: `body` the tokens inside the braces.
    pub fn rules(body: TokenStream<A>, at: DefAt) -> MacroDef<A> {
        let alloc = body.allocator().clone();
        MacroDef::make(MacroKind::Rules, body, None, at, alloc)
    }

    /// `macro m { body }` (`params` `None`), or `macro m(params) { body }`.
    pub fn macro2(params: Option<TokenStream<A>>, body: TokenStream<A>, at: DefAt) -> MacroDef<A> {
        let alloc = body.allocator().clone();
        MacroDef::make(MacroKind::Macro2, body, params, at, alloc)
    }

    pub fn builtin_fn(which: BuiltinFn, at: DefAt, alloc: A) -> MacroDef<A> {
        let body = TokenStream::new_in(alloc.clone());
        MacroDef::make(MacroKind::BuiltinFn(which), body, None, at, alloc)
    }

    pub fn builtin_attr(which: BuiltinAttr, at: DefAt, alloc: A) -> MacroDef<A> {
        let body = TokenStream::new_in(alloc.clone());
        MacroDef::make(MacroKind::BuiltinAttr(which), body, None, at, alloc)
    }

    pub fn builtin_derive(which: BuiltinDerive, at: DefAt, alloc: A) -> MacroDef<A> {
        let body = TokenStream::new_in(alloc.clone());
        MacroDef::make(MacroKind::BuiltinDerive(which), body, None, at, alloc)
    }

    /// The proc macro `name` of the proc-macro crate `krate`; a derive's `helpers` are the
    /// attributes it declares (`#[proc_macro_derive(Name, attributes(helper, ..))]`).
    pub fn proc_macro(
        krate: CrateItemKey,
        kind: ProcKind,
        name: &str,
        helpers: &[&str],
        at: DefAt,
        alloc: A,
    ) -> Result<MacroDef<A>, StreamError> {
        let mut names = Vec::new_in(alloc.clone());
        let name = push_text(&mut names, name, Table::Text)?;
        let mut helper_texts = Vec::with_capacity_in(helpers.len(), alloc.clone());
        for helper in helpers {
            helper_texts.push(push_text(&mut names, helper, Table::Text)?);
        }
        let transparency = at.transparency.unwrap_or(Transparency::Opaque);
        let mut def = MacroDef {
            kind: MacroKind::ProcMacro { krate, kind, name },
            body: TokenStream::new_in(alloc),
            params: None,
            names,
            helpers: helper_texts,
            at,
            transparency,
            content: MacroContent(ContentId([0; 32])),
        };
        def.content = def.hash();
        Ok(def)
    }

    fn make(
        kind: MacroKind,
        body: TokenStream<A>,
        params: Option<TokenStream<A>>,
        at: DefAt,
        alloc: A,
    ) -> MacroDef<A> {
        let own = match kind {
            MacroKind::Rules | MacroKind::BuiltinFn(_) | MacroKind::BuiltinAttr(_) => {
                Transparency::SemiOpaque
            }
            MacroKind::Macro2 | MacroKind::BuiltinDerive(_) | MacroKind::ProcMacro { .. } => {
                Transparency::Opaque
            }
        };
        let mut def = MacroDef {
            kind,
            body,
            params,
            names: Vec::new_in(alloc.clone()),
            helpers: Vec::new_in(alloc),
            at,
            transparency: at.transparency.unwrap_or(own),
            content: MacroContent(ContentId([0; 32])),
        };
        def.content = def.hash();
        def
    }
}

impl<A: Allocator> MacroDef<A> {
    pub fn kind(&self) -> MacroKind {
        self.kind
    }

    /// The rules of a declarative macro, or a `macro m(..)`'s body; empty for the others.
    pub fn body(&self) -> &TokenStream<A> {
        &self.body
    }

    pub fn params(&self) -> Option<&TokenStream<A>> {
        self.params.as_ref()
    }

    pub fn at(&self) -> DefAt {
        self.at
    }

    pub fn edition(&self) -> Edition {
        self.at.edition
    }

    pub fn site(&self) -> DefSite {
        self.at.site
    }

    pub fn transparency(&self) -> Transparency {
        self.transparency
    }

    pub fn content(&self) -> MacroContent {
        self.content
    }

    /// A proc macro's export name.
    pub fn proc_name(&self) -> Option<&str> {
        match self.kind {
            MacroKind::ProcMacro { name, .. } => Some(name.of(&self.names)),
            _ => None,
        }
    }

    /// A derive's helper attributes, in the order declared.
    pub fn helpers(&self) -> impl Iterator<Item = &str> + '_ {
        self.helpers.iter().map(|text| text.of(&self.names))
    }

    /// The mark this definition's expansion puts on a token, at `transparency`.
    pub(crate) fn mark(&self, transparency: Transparency) -> Mark {
        Mark { of: MarkOf::This, transparency, edition: self.at.edition, site: self.at.site }
    }

    fn hash(&self) -> MacroContent {
        let mut hasher = Hasher::new("blaze v2 expand macro content 1");
        match self.kind {
            MacroKind::Rules => hasher.put(&[0]),
            MacroKind::Macro2 => hasher.put(&[1]),
            MacroKind::BuiltinFn(which) => {
                hasher.put(&[2]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::BuiltinAttr(which) => {
                hasher.put(&[3]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::BuiltinDerive(which) => {
                hasher.put(&[4]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::ProcMacro { krate, kind, name } => {
                hasher.put(&[5, kind as u8]);
                hasher.id(&krate.0);
                hasher.part(name.of(&self.names).as_bytes());
            }
        }
        self.body.encode(&mut hasher);
        match &self.params {
            None => hasher.put(&[0]),
            Some(params) => {
                hasher.put(&[1]);
                params.encode(&mut hasher);
            }
        }
        hasher.put(&crate::tokens::index(self.helpers.len()).to_be_bytes());
        for helper in self.helpers() {
            hasher.part(helper.as_bytes());
        }
        let export = u8::from(self.at.export == Export::Exported);
        let inner = u8::from(self.at.local_inner == LocalInner::Yes);
        hasher.put(&[self.at.edition.byte(), export, inner, self.transparency.byte()]);
        MacroContent(hasher.finish())
    }
}
