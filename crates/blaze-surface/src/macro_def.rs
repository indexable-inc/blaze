//! A macro's definition as the item layer keeps it (docs/v2/plan/design/bottom.md, the
//! `MacroDef` row): its words ([`MacroKind`], the builtin names, [`ProcKind`], [`Transparency`],
//! [`DefSite`]), each defined once for name resolution and for blaze-expand (rule 30), and the
//! [`MacroDef`] record, written once and read in place ([`MacroDefView`]).
//!
//! The record holds everything blaze-expand's `MacroDefinition` holds. Its token streams (a
//! declarative macro's rules, a `macro m(..)`'s parameters) are records of their own, named here
//! by their digest ([`MacroDefHead::body`], [`MacroDefHead::params`]): a resolver that reads a
//! macro's kind, name and flags never reads its tokens, and two definitions with one body share
//! its record.

// `fmt` is named by the `words!` macro of kind.rs, expanded in this module.
use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Writer, WriterOpts};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::{AVec, Alloc};

use crate::column::{
    Column, NONE, RecordBytes, Strings, Text, at_of, at_of_record, check_text, field_at,
    finish_with, key_of_word, key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{CallShape, Edition, Place, PlaceTag, byte_enum};

byte_enum! {
    /// The builtin function-like macros, by name.
    pub enum BuiltinFn, refused as "a byte that is no builtin function-like macro" {
        FormatArgs = 0 => "format_args",
        FormatArgsNl = 1 => "format_args_nl",
        ConstFormatArgs = 2 => "const_format_args",
        Concat = 3 => "concat",
        Stringify = 4 => "stringify",
        /// `core::panic!` and `std::panic!`: `$crate` tells them apart.
        Panic = 5 => "panic",
        Unreachable = 6 => "unreachable",
        Cfg = 7 => "cfg",
        Env = 8 => "env",
        OptionEnv = 9 => "option_env",
        IncludeStr = 10 => "include_str",
        IncludeBytes = 11 => "include_bytes",
        Line = 12 => "line",
        Column = 13 => "column",
        File = 14 => "file",
        ModulePath = 15 => "module_path",
        CompileError = 16 => "compile_error",
        Include = 17 => "include",
        Assert = 18 => "assert",
        ConcatBytes = 19 => "concat_bytes",
        Asm = 20 => "asm",
        GlobalAsm = 21 => "global_asm",
        NakedAsm = 22 => "naked_asm",
        CfgSelect = 23 => "cfg_select",
        TraceMacros = 24 => "trace_macros",
        LogSyntax = 25 => "log_syntax",
        /// core's `pattern_type!(T is PAT)` (library/core/src/pat.rs): written
        /// `builtin # pattern_type(T is PAT)`, the form the parser reads a pattern type in.
        PatternType = 26 => "pattern_type",
    }
}

impl BuiltinFn {
    /// The builtin a library definition names: the argument of its `#[rustc_builtin_macro(..)]`,
    /// or the macro's own name when the attribute has none. The library's `core::panic!` is
    /// `rustc_builtin_macro(core_panic)` and `std::panic!` is `rustc_builtin_macro(std_panic)`:
    /// both are [`BuiltinFn::Panic`], whose expansion names `$crate::panic::panic_2015` or
    /// `panic_2021`, and `$crate` is the defining crate, so one form serves both.
    pub fn of_library(name: &str) -> Option<BuiltinFn> {
        match name {
            "core_panic" | "std_panic" => Some(BuiltinFn::Panic),
            _ => BuiltinFn::ALL.into_iter().find(|it| it.name() == name),
        }
    }
}

byte_enum! {
    /// The builtin attribute macros, by name.
    pub enum BuiltinAttr, refused as "a byte that is no builtin attribute macro" {
        Test = 0 => "test",
        Bench = 1 => "bench",
        TestCase = 2 => "test_case",
        GlobalAllocator = 3 => "global_allocator",
        CfgEval = 4 => "cfg_eval",
        Derive = 5 => "derive",
        DeriveConst = 6 => "derive_const",
        AllocErrorHandler = 7 => "alloc_error_handler",
    }
}

byte_enum! {
    /// The builtin derives, by name.
    pub enum BuiltinDerive, refused as "a byte that is no builtin derive" {
        Copy = 0 => "Copy",
        Clone = 1 => "Clone",
        Default = 2 => "Default",
        Debug = 3 => "Debug",
        Hash = 4 => "Hash",
        Ord = 5 => "Ord",
        PartialOrd = 6 => "PartialOrd",
        Eq = 7 => "Eq",
        PartialEq = 8 => "PartialEq",
        /// `#[derive(ConstParamTy)]`: an impl of `core::marker::ConstParamTy_`.
        ConstParamTy = 9 => "ConstParamTy",
    }
}

byte_enum! {
    /// How a proc macro is called: `#[proc_macro]`, `#[proc_macro_attribute]` or
    /// `#[proc_macro_derive]`.
    pub enum ProcKind, refused as "a byte that is no proc macro kind" {
        FnLike = 0 => "function-like",
        Attr = 1 => "attribute",
        Derive = 2 => "derive",
    }
}

byte_enum! {
    /// How an expansion's mark resolves names (rustc's `Transparency`).
    pub enum Transparency, refused as "a byte that is no transparency" {
        /// Resolved at the call site (`Span::call_site`, `macro` hygiene opt-outs).
        Transparent = 0 => "transparent",
        /// Locals, labels and `$crate` at the definition, everything else at the call site
        /// (`macro_rules!`, `Span::mixed_site`).
        SemiOpaque = 1 => "semi-opaque",
        /// Resolved at the definition (`macro`, `Span::def_site`).
        Opaque = 2 => "opaque",
    }
}

/// What kind of macro a definition is. A proc macro's export name is the definition's name
/// ([`MacroDefHead::name`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MacroKind {
    /// `macro_rules! m { rules }`: its body is the rules.
    Rules,
    /// `macro m { rules }`, or `macro m(params) { body }`.
    Macro2,
    BuiltinFn(BuiltinFn),
    BuiltinAttr(BuiltinAttr),
    BuiltinDerive(BuiltinDerive),
    /// A proc macro of the proc-macro crate `krate`.
    ProcMacro { krate: CrateItemKey, kind: ProcKind },
}

byte_enum! {
    /// [`MacroKind`]'s byte in a record; the builtin's or proc macro's own byte is beside it.
    pub(crate) enum MacroKindTag, refused as "a byte that is no macro kind" {
        Rules = 0 => "macro_rules",
        Macro2 = 1 => "macro",
        BuiltinFn = 2 => "builtin function-like",
        BuiltinAttr = 3 => "builtin attribute",
        BuiltinDerive = 4 => "builtin derive",
        ProcMacro = 5 => "proc macro",
    }
}

impl MacroKind {
    /// The kind of call the macro takes; `None` for a declarative macro, whose rules say.
    pub fn shape(self) -> Option<CallShape> {
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

    /// The transparency the kind's expansions take unless `#[rustc_macro_transparency]` says
    /// otherwise.
    pub fn own_transparency(self) -> Transparency {
        match self {
            MacroKind::Rules | MacroKind::BuiltinFn(_) | MacroKind::BuiltinAttr(_) => {
                Transparency::SemiOpaque
            }
            MacroKind::Macro2 | MacroKind::BuiltinDerive(_) | MacroKind::ProcMacro { .. } => {
                Transparency::Opaque
            }
        }
    }

    /// Its bytes as a record holds them: the tag, the builtin's or proc kind's byte (0 for a
    /// declarative macro) and the proc macro's crate (zero bytes for the others), so two
    /// builders write equal bytes.
    pub(crate) fn to_record(self) -> (MacroKindTag, u8, ContentId) {
        let none = ContentId([0; 32]);
        match self {
            MacroKind::Rules => (MacroKindTag::Rules, 0, none),
            MacroKind::Macro2 => (MacroKindTag::Macro2, 0, none),
            MacroKind::BuiltinFn(which) => (MacroKindTag::BuiltinFn, which.byte(), none),
            MacroKind::BuiltinAttr(which) => (MacroKindTag::BuiltinAttr, which.byte(), none),
            MacroKind::BuiltinDerive(which) => (MacroKindTag::BuiltinDerive, which.byte(), none),
            MacroKind::ProcMacro { krate, kind } => (MacroKindTag::ProcMacro, kind.byte(), krate.0),
        }
    }

    /// What [`MacroKind::to_record`] wrote; `None` for bytes it does not write.
    pub(crate) fn from_record(tag: MacroKindTag, which: u8, krate: ContentId) -> Option<MacroKind> {
        let none = krate.0 == [0; 32];
        let kind = match tag {
            MacroKindTag::Rules => MacroKind::Rules,
            MacroKindTag::Macro2 => MacroKind::Macro2,
            MacroKindTag::BuiltinFn => MacroKind::BuiltinFn(BuiltinFn::from_byte(which)?),
            MacroKindTag::BuiltinAttr => MacroKind::BuiltinAttr(BuiltinAttr::from_byte(which)?),
            MacroKindTag::BuiltinDerive => {
                MacroKind::BuiltinDerive(BuiltinDerive::from_byte(which)?)
            }
            MacroKindTag::ProcMacro => {
                return Some(MacroKind::ProcMacro {
                    krate: CrateItemKey(krate),
                    kind: ProcKind::from_byte(which)?,
                });
            }
        };
        let declarative = matches!(tag, MacroKindTag::Rules | MacroKindTag::Macro2);
        (none && (!declarative || which == 0)).then_some(kind)
    }
}

// SAFETY: closed enums of names and a crate key.
unsafe impl Portable for MacroKind {}

/// Where a macro is defined: what `$crate` and a def-site path resolve against.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DefSite {
    /// The crate of the definition, what `$crate` names.
    pub krate: CrateItemKey,
    /// The module of the definition, for a macro whose names resolve there (`macro`, and an
    /// opaque mark); `None` for `macro_rules!`, whose def-site names are its crate's.
    pub module: Option<ItemKey>,
}

// SAFETY: a crate key and a module's item key.
unsafe impl Portable for DefSite {}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct MacroDefRoot: 2 {
        pub(crate) item: ContentId,
        pub(crate) content: ContentId,
        pub(crate) body: ContentId,
        pub(crate) params: ContentId,
        pub(crate) origin: ContentId,
        pub(crate) krate: ContentId,
        pub(crate) site_krate: ContentId,
        pub(crate) site_module: ContentId,
        pub(crate) name: Text,
        pub(crate) index: u32,
        pub(crate) helpers: blaze_format::Slice<Text>,
        pub(crate) strings: blaze_format::Slice<u8>,
        pub(crate) kind: MacroKindTag,
        pub(crate) which: u8,
        pub(crate) place: PlaceTag,
        pub(crate) edition: Edition,
        pub(crate) transparency: Transparency,
        pub(crate) written: Transparency,
        pub(crate) placed: bool,
        pub(crate) exported: bool,
        pub(crate) local_inner: bool,
        pub(crate) has_body: bool,
        pub(crate) has_params: bool,
        pub(crate) has_written: bool,
    }
}

/// What a macro definition is, besides its helper attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroDefHead<'a> {
    /// The macro's item.
    pub item: ItemKey,
    /// Its name; a proc macro's export name.
    pub name: &'a str,
    /// The hash of what the definition is (blaze-expand's `MacroContent`): what an
    /// `Expansion` is keyed by, never the item key, so a moved macro keeps its expansions.
    pub content: ContentId,
    /// Where it is written; `None` for a builtin macro.
    pub place: Option<Place>,
    pub kind: MacroKind,
    pub site: DefSite,
    pub edition: Edition,
    /// `#[macro_export]`.
    pub exported: bool,
    /// `#[macro_export(local_inner_macros)]`.
    pub local_inner: bool,
    /// `#[rustc_macro_transparency = ".."]`, as written.
    pub written_transparency: Option<Transparency>,
    /// The transparency its expansions take: the written one, else the kind's own.
    pub transparency: Transparency,
    /// The digest of the record of the body's tokens (the rules, or a `macro m(..)`'s body);
    /// `None` when the body has no tokens and no contexts, as a builtin's and a proc macro's.
    pub body: Option<ContentId>,
    /// The digest of the record of a `macro m(params) { .. }`'s parameters.
    pub params: Option<ContentId>,
}

/// The writer of one macro's [`MacroDef`] record: its head and, for a derive, its helper
/// attributes in the order `attributes(..)` lists them (the source's order, which fixes the
/// bytes).
pub struct MacroDef<'a, 'b> {
    head: MacroDefHead<'a>,
    helpers: AVec<'b, Text>,
    staging: Strings<'b>,
    alloc: Alloc<'b>,
}

impl<'a, 'b> MacroDef<'a, 'b> {
    pub fn new_in(head: MacroDefHead<'a>, alloc: Alloc<'b>) -> MacroDef<'a, 'b> {
        MacroDef { head, helpers: AVec::new_in(alloc), staging: Strings::new_in(alloc), alloc }
    }

    /// A derive's helper attribute.
    pub fn helper(&mut self, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "MacroDef strings")?;
        self.helpers.push(name);
        Ok(())
    }

    pub fn finish(mut self) -> Result<RecordBytes<'b>, BuildError> {
        let mut strings = Strings::new_in(self.alloc);
        let name = strings.push(self.head.name, "MacroDef strings")?;
        for text in &mut self.helpers {
            *text = strings.moved(&self.staging, *text, "MacroDef strings")?;
        }
        let mut out =
            Writer::new(WriterOpts { schema: <MacroDefRoot as blaze_format::Record>::SCHEMA, alloc: self.alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let helpers = Column::push(&mut out, &self.helpers);
        let head = self.head;
        let none = ContentId([0; 32]);
        let (place, origin, index) = match head.place {
            Some(place) => place.to_record(),
            None => (PlaceTag::File, none, NONE),
        };
        let (kind, which, krate) = head.kind.to_record();
        Ok(finish_with(out, |at| MacroDefRoot {
            item: head.item.0,
            content: head.content,
            body: head.body.unwrap_or(none),
            params: head.params.unwrap_or(none),
            origin,
            krate,
            site_krate: head.site.krate.0,
            site_module: key_word(head.site.module),
            name,
            index,
            helpers: helpers.slice(at, offset_of!(MacroDefRoot, helpers)),
            strings: text.slice(at, offset_of!(MacroDefRoot, strings)),
            kind,
            which,
            place,
            edition: head.edition,
            transparency: head.transparency,
            written: head.written_transparency.unwrap_or(Transparency::Transparent),
            placed: head.place.is_some(),
            exported: head.exported,
            local_inner: head.local_inner,
            has_body: head.body.is_some(),
            has_params: head.params.is_some(),
            has_written: head.written_transparency.is_some(),
        }))
    }
}

/// A [`MacroDef`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct MacroDefView<'a> {
    head: MacroDefHead<'a>,
    helpers: &'a [Text],
    strings: &'a str,
}

impl<'a> MacroDefView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<MacroDefView<'a>, FormatError> {
        let root = blaze_format::open::<MacroDefRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        check_text(strings, root.name, blaze_format::Verified::get(root), buffer)?;
        let helpers = root.helpers.follow(buffer)?;
        for text in helpers {
            check_text(strings, *text, text, buffer)?;
        }
        let at = at_of_record(blaze_format::Verified::get(root), buffer);
        let kind = MacroKind::from_record(root.kind, root.which, root.krate).ok_or(
            FormatError::Invalid {
                at: field_at(at, offset_of!(MacroDefRoot, which)),
                what: "a macro kind's byte or crate that its kind does not write",
            },
        )?;
        let given = |has: bool, id: ContentId| has.then_some(id);
        Ok(MacroDefView {
            head: MacroDefHead {
                item: ItemKey(root.item),
                name: text_of(strings, root.name),
                content: root.content,
                place: root.placed.then(|| Place::from_record(root.place, root.origin, root.index)),
                kind,
                site: DefSite {
                    krate: CrateItemKey(root.site_krate),
                    module: key_of_word(root.site_module),
                },
                edition: root.edition,
                exported: root.exported,
                local_inner: root.local_inner,
                written_transparency: root.has_written.then_some(root.written),
                transparency: root.transparency,
                body: given(root.has_body, root.body),
                params: given(root.has_params, root.params),
            },
            helpers,
            strings,
        })
    }

    pub fn head(&self) -> MacroDefHead<'a> {
        self.head
    }

    /// A derive's helper attributes, in the order it lists them.
    pub fn helpers(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.helpers.iter().map(move |text| text_of(strings, *text))
    }
}
