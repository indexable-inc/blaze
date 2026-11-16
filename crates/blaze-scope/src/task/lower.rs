//! Source lowering (bottom.md 6, "Names"): a crate's text to the records the names tasks read.
//!
//! - [`Cfg`]: one stated configuration, a value with a [`Cfg::digest`] a caller puts in a task's
//!   key, and the evaluation of a `cfg` predicate and of an item's attributes under it
//!   ([`item_attrs`], [`file_attrs`]).
//! - [`walk`]: the module tree from the crate's files, read through [`Files`]: which file each
//!   `mod x;` is, by the rules rustc follows (`x.rs`, `x/mod.rs`, `#[path]`).
//! - [`lower`]: one fragment (a module's own items, or one macro call's expansion) to its
//!   [`DeclsBuilder`] and what a driver keeps beside it: the macros it defines, the calls it
//!   makes, enums' variants, impls, lang items, the modules a macro wrote.
//! - [`crate_root`]: the `CrateRoot` from the lowered root and the graph's [`RootPlan`].
//!
//! Every key is a function of the crate's key and the item's place in the text ([`key_of`]).
//! What the lowering cannot read is a typed note ([`LowerNote`], [`WalkNote`], [`RootNote`])
//! in the output, never dropped. No IO: the text and the files come in through the caller.

#![blaze::sans_io]

use core::fmt;

use blaze_file_items::{Outline, syntax_kind};
use blaze_hash::{HashMap, HashSet};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ids::Hasher;
use blaze_own::{AString, AVec, Alloc};
use blaze_surface::{
    AttrKind, BuiltinAttr, BuiltinDerive, CallShape, ChildModDecl, CrateRootBuilder, CrateRootHead, DeclaredItem,
    DeclsBuilder, Def, DefKind, Dep, Edition, ExternCrateDecl, ExternTarget, Item, ItemDetail, ItemIndex, ItemKind,
    MacroCallDecl, MacroRulesDecl, MacroShapes, Namespace, Path, PathKind, ProcKind, Range, Rename, UseDecl, UseForm,
    WrittenVis,
};

use super::Variant;

// ---------------------------------------------------------------------------------------------
// Keys.

/// The domain of every key [`make_key`] derives.
const KEY_DOMAIN: &str = "blaze pipeline example key 1";

/// The key of the parts `parts` in the crate `krate`: each part length-prefixed, hashed in
/// [`KEY_DOMAIN`], the first 8 bytes the crate's prefix, and the inner bit clear.
pub fn make_key(krate: CrateItemKey, parts: &[&[u8]]) -> ItemKey {
    let mut hasher = Hasher::new(KEY_DOMAIN);
    for part in parts {
        hasher.part(part);
    }
    let mut id = hasher.finish().0;
    id[..8].copy_from_slice(&krate.prefix());
    id[8] &= !ItemKey::INNER;
    ItemKey(ContentId(id))
}

/// The key of the `index`th entry of `parent` (a module, a call or an item) of kind `tag` and
/// name `name`.
pub fn key_of(krate: CrateItemKey, parent: ItemKey, tag: &str, name: &str, index: u32) -> ItemKey {
    make_key(krate, &[&parent.0.0, tag.as_bytes(), name.as_bytes(), &index.to_le_bytes()])
}

/// The key of the crate's root module.
pub fn root_key(krate: CrateItemKey) -> ItemKey {
    make_key(krate, &[b"crate root"])
}

/// The tag an item's key is derived with: its kind's name as written in [`ItemKind`].
const fn kind_tag(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Function => "Function",
        ItemKind::Struct => "Struct",
        ItemKind::Enum => "Enum",
        ItemKind::Union => "Union",
        ItemKind::Trait => "Trait",
        ItemKind::Impl => "Impl",
        ItemKind::Const => "Const",
        ItemKind::Static => "Static",
        ItemKind::TypeAlias => "TypeAlias",
        ItemKind::Module => "Module",
        ItemKind::MacroRules => "MacroRules",
        ItemKind::Use => "Use",
        ItemKind::ExternCrate => "ExternCrate",
        ItemKind::Variant => "Variant",
        ItemKind::Field => "Field",
        ItemKind::Macro2 => "Macro2",
        ItemKind::TraitAlias => "TraitAlias",
        ItemKind::ExternBlock => "ExternBlock",
        ItemKind::MacroCall => "MacroCall",
        ItemKind::Error => "Error",
        ItemKind::Unhandled => "Unhandled",
    }
}

// ---------------------------------------------------------------------------------------------
// cfg: one stated configuration.

/// Names a predicate may hold that are false on purpose in every configuration made here.
pub const KNOWN_FALSE: &[&str] = &[
    "test", "debug_assertions", "doc", "doctest", "miri", "bootstrap", "rustdoc", "clippy",
    "no_global_oom_handling", "no_rc", "no_sync", "overflow_checks", "ub_checks", "fuzzing",
    "kani", "emscripten_wasm_eh", "windows", "target_has_reliable_f16",
    "target_has_reliable_f16_math", "target_has_reliable_f128", "target_has_reliable_f128_math",
    "sanitize", "panic_immediate_abort", "optimize_for_size", "no_fp_fmt_parse", "feature",
];

/// The target a crate is configured for, as a value.
#[derive(Clone, Copy, Debug)]
pub struct Target<'s> {
    pub os: &'s str,
    pub arch: &'s str,
    pub family: &'s str,
    pub pointer_width: &'s str,
    pub endian: &'s str,
    pub env: &'s str,
    pub vendor: &'s str,
    pub debug_assertions: bool,
    /// rustc's `target_has_reliable_f16` (true on aarch64 but Windows).
    pub reliable_f16: bool,
    /// The widths `target_has_atomic` holds (`"8"`, ..., `"ptr"`).
    pub atomic_widths: &'s [&'s str],
    /// The target features on.
    pub target_features: &'s [&'s str],
}

impl Target<'static> {
    /// `aarch64-apple-darwin`, a dev build: what the owner's `blaze run` builds for.
    pub const AARCH64_APPLE_DARWIN: Target<'static> = Target {
        os: "macos",
        arch: "aarch64",
        family: "unix",
        pointer_width: "64",
        endian: "little",
        env: "",
        vendor: "apple",
        debug_assertions: true,
        reliable_f16: true,
        atomic_widths: &["8", "16", "32", "64", "ptr"],
        target_features: &["neon", "fp-armv8", "aes", "sha2", "crc", "lse"],
    };
}

impl fmt::Display for Target<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "target_os=\"{}\", target_arch=\"{}\", {}, target_pointer_width=\"{}\", target_endian=\"{}\", target_vendor=\"{}\", debug_assertions {}",
            self.os,
            self.arch,
            self.family,
            self.pointer_width,
            self.endian,
            self.vendor,
            if self.debug_assertions { "on" } else { "off" }
        )
    }
}

/// One configuration: the atoms it sets, sorted, a name as `(name, None)` and a
/// `key = "value"` pair as `(key, Some(value))` (blaze-unit-graph's `Unit::cfg` as it is). Any
/// other atom is false.
#[derive(Clone, Debug)]
pub struct Cfg<'s, 'a> {
    atoms: AVec<'a, (&'s str, Option<&'s str>)>,
}

/// The most nested a predicate this reads may be (`all(any(not(..)))`): past it, unread.
pub const MOST_CFG_DEPTH: usize = 32;

const CFG_DOMAIN: &str = "blaze-scope cfg 2";

impl<'s, 'a> Cfg<'s, 'a> {
    /// The configuration of `target` with `features` on.
    pub fn of(target: &Target<'s>, features: &[&'s str], alloc: Alloc<'a>) -> Cfg<'s, 'a> {
        let mut atoms: AVec<'a, (&'s str, Option<&'s str>)> = AVec::new_in(alloc);
        let family = target.family;
        atoms.push((family, None));
        atoms.push(("target_thread_local", None));
        if target.debug_assertions {
            atoms.push(("debug_assertions", None));
        }
        if target.reliable_f16 {
            atoms.push(("target_has_reliable_f16", None));
            atoms.push(("target_has_reliable_f16_math", None));
        }
        let pairs: [(&'s str, &'s str); 11] = [
            ("target_os", target.os),
            ("target_arch", target.arch),
            ("target_family", family),
            ("target_pointer_width", target.pointer_width),
            ("target_endian", target.endian),
            ("target_env", target.env),
            ("target_vendor", target.vendor),
            ("target_abi", ""),
            ("panic", "unwind"),
            ("fmt_debug", "full"),
            ("relocation_model", "pic"),
        ];
        atoms.extend(pairs.iter().map(|&(key, value)| (key, Some(value))));
        for width in target.atomic_widths {
            atoms.push(("target_has_atomic", Some(width)));
            atoms.push(("target_has_atomic_load_store", Some(width)));
            atoms.push(("target_has_atomic_equal_alignment", Some(width)));
        }
        atoms.push(("target_has_atomic", None));
        atoms.push(("target_has_atomic_load_store", None));
        atoms.push(("target_has_atomic_equal_alignment", None));
        for &feature in target.target_features {
            atoms.push(("target_feature", Some(feature)));
        }
        for &feature in features {
            atoms.push(("feature", Some(feature)));
        }
        Cfg::from_owned(atoms)
    }

    /// The configuration that sets `atoms` and nothing else: blaze-unit-graph's `Unit::cfg`,
    /// each atom borrowed (`feature = ".."` and `debug_assertions` among them).
    pub fn from_atoms(atoms: &[(&'s str, Option<&'s str>)], alloc: Alloc<'a>) -> Cfg<'s, 'a> {
        Cfg::from_owned(AVec::from_slice_in(atoms, alloc))
    }

    /// The atoms it sets, sorted, each once: what [`Cfg::from_atoms`] makes it again from.
    pub fn atoms(&self) -> &[(&'s str, Option<&'s str>)] {
        &self.atoms
    }

    /// The one constructor: `atoms` sorted and each kept once.
    fn from_owned(mut atoms: AVec<'a, (&'s str, Option<&'s str>)>) -> Cfg<'s, 'a> {
        atoms.sort_unstable();
        atoms.dedup();
        Cfg { atoms }
    }

    /// What the configuration is, as a key's part: the sorted atoms, so equal atoms give equal
    /// digests whichever constructor made them.
    pub fn digest(&self) -> ContentId {
        let mut hasher = Hasher::new(CFG_DOMAIN);
        hasher.update(&(self.atoms.len() as u64).to_le_bytes());
        for (name, value) in self.atoms.iter() {
            hasher.part(name.as_bytes());
            match value {
                None => hasher.update(&[0]),
                Some(value) => hasher.update(&[1]).part(value.as_bytes()),
            };
        }
        hasher.finish()
    }

    /// Whether the name `prefix` `name` is set (`true` and `false` are themselves).
    fn has_name(&self, prefix: &str, name: &str) -> bool {
        if prefix.is_empty() {
            match name {
                "true" => return true,
                "false" => return false,
                _ => {}
            }
        }
        self.has(prefix, name, None)
    }

    /// Whether the key `prefix` `key` is set to `value`.
    fn has_pair(&self, prefix: &str, key: &str, value: &str) -> bool {
        self.has(prefix, key, Some(value))
    }

    /// Whether the atom (`prefix` `name`, `value`) is set: the atoms' own order, the name's
    /// bytes then the value (`None` first).
    fn has(&self, prefix: &str, name: &str, value: Option<&str>) -> bool {
        self.atoms
            .binary_search_by(|(n, v)| n.bytes().cmp(prefix.bytes().chain(name.bytes())).then_with(|| v.cmp(&value)))
            .is_ok()
    }

    /// The predicate's value, `None` when it does not read as one (a token or form this does
    /// not know, or nested past [`MOST_CFG_DEPTH`]).
    pub fn eval(&self, text: &str) -> Option<bool> {
        #[derive(Clone, Copy)]
        enum Op {
            All,
            Any,
            Not,
        }
        #[derive(Clone, Copy)]
        struct Frame {
            op: Op,
            /// Its predicates' names are read with `target_` before them.
            target: bool,
            value: bool,
            count: u32,
        }
        fn finish(frame: Frame) -> Option<bool> {
            match frame.op {
                Op::All | Op::Any => Some(frame.value),
                Op::Not => (frame.count == 1).then_some(frame.value),
            }
        }
        let empty = Frame { op: Op::All, target: false, value: true, count: 0 };
        let mut frames = [empty; MOST_CFG_DEPTH];
        let mut depth = 0usize;
        let mut lex = CLex { text, at: 0, peeked: None, bad: false };
        'pred: loop {
            let Some(CTok::Ident(name)) = lex.next() else { return None };
            let target = depth > 0 && frames[depth - 1].target;
            let prefix = if target { "target_" } else { "" };
            let mut value = match lex.peek() {
                Some(CTok::Open) => {
                    lex.next();
                    let op = match name {
                        "all" | "target" => Op::All,
                        "any" => Op::Any,
                        "not" => Op::Not,
                        _ => return None,
                    };
                    if depth == MOST_CFG_DEPTH {
                        return None;
                    }
                    frames[depth] = Frame { op, target: target || name == "target", value: matches!(op, Op::All), count: 0 };
                    depth += 1;
                    if lex.peek() != Some(CTok::Close) {
                        continue 'pred;
                    }
                    lex.next();
                    depth -= 1;
                    finish(frames[depth])?
                }
                Some(CTok::Eq) => {
                    lex.next();
                    let Some(CTok::Str(value)) = lex.next() else { return None };
                    self.has_pair(prefix, name, value)
                }
                _ => self.has_name(prefix, name),
            };
            // A predicate is read: fold it into the lists it ends.
            loop {
                if depth == 0 {
                    return (lex.next().is_none() && !lex.bad).then_some(value);
                }
                let frame = &mut frames[depth - 1];
                match frame.op {
                    Op::All => frame.value &= value,
                    Op::Any => frame.value |= value,
                    Op::Not => frame.value = !value,
                }
                frame.count += 1;
                match lex.next() {
                    Some(CTok::Comma) => {
                        if lex.peek() != Some(CTok::Close) {
                            continue 'pred;
                        }
                        lex.next();
                    }
                    Some(CTok::Close) => {}
                    _ => return None,
                }
                depth -= 1;
                value = finish(frames[depth])?;
            }
        }
    }

    /// Reads one attribute's content (`cfg(..)`, `path = ".."`, `derive(..)`, ...) into
    /// `attrs`, a true `cfg_attr`'s attributes in its place, in order.
    pub fn attr<'t>(&self, attrs: &mut Attrs<'t, '_>, content: &'t str, stack: &mut AVec<'_, &'t str>) {
        let base = stack.len();
        let mut next = Some(content);
        loop {
            let content = match next.take() {
                Some(it) => it,
                None if stack.len() > base => stack.pop().expect("the stack is longer than its base"),
                None => break,
            };
            let name = prelude_attr(head(content));
            match name {
                "cfg" => match self.eval(args(content)) {
                    Some(true) => {}
                    Some(false) => attrs.cfg_ok = false,
                    None => {
                        attrs.cfg_ok = false;
                        attrs.cfg_unread += 1;
                    }
                },
                "cfg_attr" => {
                    attrs.cfg_attrs += 1;
                    let mut parts = SplitTop::new(args(content));
                    let Some(predicate) = parts.next() else { continue };
                    let on = match self.eval(predicate) {
                        Some(on) => on,
                        None => {
                            attrs.cfg_unread += 1;
                            false
                        }
                    };
                    if on {
                        let start = stack.len();
                        for part in parts {
                            let part = part.trim();
                            if !part.is_empty() {
                                stack.push(part);
                            }
                        }
                        stack[start..].reverse();
                    }
                }
                "unsafe" => next = Some(args(content).trim()),
                "path" => attrs.path = string_value(content),
                "derive" | "derive_const" => {
                    for path in SplitTop::new(args(content)) {
                        let path = path.trim();
                        if !path.is_empty() {
                            attrs.derives.push(path);
                        }
                    }
                }
                "macro_export" => {
                    attrs.macro_export = true;
                    attrs.local_inner |= args(content).contains("local_inner_macros");
                }
                "macro_use" => attrs.macro_use = true,
                "lang" => attrs.lang = string_value(content),
                "rustc_builtin_macro" => {
                    attrs.builtin = true;
                    let arg = SplitTop::new(args(content)).next().map_or("", str::trim);
                    if !arg.is_empty() {
                        attrs.builtin_name = Some(arg);
                    }
                }
                "proc_macro" | "proc_macro_attribute" | "proc_macro_derive" => {
                    if let Some(it) = ProcAttr::read(content) {
                        attrs.proc_macro = Some(it);
                    }
                }
                "prelude_import" => attrs.prelude_import = true,
                "no_implicit_prelude" => attrs.no_implicit_prelude = true,
                "no_std" => attrs.no_std = true,
                "no_core" => attrs.no_core = true,
                "register_tool" => {
                    for tool in SplitTop::new(args(content)) {
                        let tool = tool.trim();
                        if !tool.is_empty() {
                            attrs.tools.push(tool);
                        }
                    }
                }
                _ => {
                    // `#![blaze::arbitrary_ints]` (`blaze :: arbitrary_ints` in an expansion's
                    // text) is a tool attribute: inert here, and read as a flag.
                    attrs.arbitrary_ints |=
                        name.split_once("::").is_some_and(|(tool, rest)| tool.trim() == "blaze" && rest.trim() == "arbitrary_ints");
                    // An attribute macro: a path no tool owns, or a name neither built in nor
                    // written after a derive (there it is the derive's helper, inert). The
                    // first one, outermost, takes the item (the Reference's order).
                    let tool = name.split_once("::").map(|(first, _)| TOOLS.contains(&first) || attrs.tools.contains(&first));
                    let candidate = match tool {
                        Some(tool) => !tool,
                        None => !is_builtin_attr(name) && attrs.derives.is_empty(),
                    };
                    if candidate && attrs.attr_macro.is_none() {
                        attrs.attr_macro = Some(name);
                    }
                    if !is_builtin_attr(name) {
                        attrs.others.push(name);
                    }
                }
            }
        }
    }
}

/// The tools whose attributes (`#[rustfmt::skip]`) are inert without a `register_tool`.
/// `blaze` is blaze's own tool (its extensions, rule 6).
pub const TOOLS: &[&str] = &["blaze", "clippy", "diagnostic", "miri", "rust_analyzer", "rustdoc", "rustfmt"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum CTok<'a> {
    Ident(&'a str),
    Str(&'a str),
    Open,
    Close,
    Comma,
    Eq,
}

/// A `cfg` predicate's tokens, one at a time; a byte it does not know ends them and sets `bad`.
struct CLex<'a> {
    text: &'a str,
    at: usize,
    peeked: Option<Option<CTok<'a>>>,
    bad: bool,
}

impl<'a> CLex<'a> {
    fn peek(&mut self) -> Option<CTok<'a>> {
        if let Some(it) = self.peeked {
            return it;
        }
        let it = self.lex();
        self.peeked = Some(it);
        it
    }

    fn next(&mut self) -> Option<CTok<'a>> {
        match self.peeked.take() {
            Some(it) => it,
            None => self.lex(),
        }
    }

    fn lex(&mut self) -> Option<CTok<'a>> {
        let text = self.text;
        let bytes = text.as_bytes();
        if self.bad {
            return None;
        }
        while self.at < bytes.len() {
            let c = bytes[self.at];
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.at += 1,
                // A comment is whitespace (the Reference, "Comments": "Non-doc comments are
                // interpreted as a form of whitespace"); block comments nest.
                b'/' if bytes.get(self.at + 1) == Some(&b'/') => {
                    while self.at < bytes.len() && bytes[self.at] != b'\n' {
                        self.at += 1;
                    }
                }
                b'/' if bytes.get(self.at + 1) == Some(&b'*') => {
                    let mut open = 0u32;
                    loop {
                        if self.at + 1 >= bytes.len() {
                            self.bad = true;
                            return None;
                        }
                        match (bytes[self.at], bytes[self.at + 1]) {
                            (b'/', b'*') => {
                                open += 1;
                                self.at += 2;
                            }
                            (b'*', b'/') => {
                                open -= 1;
                                self.at += 2;
                                if open == 0 {
                                    break;
                                }
                            }
                            _ => self.at += 1,
                        }
                    }
                }
                b'(' | b')' | b',' | b'=' => {
                    self.at += 1;
                    return Some(match c {
                        b'(' => CTok::Open,
                        b')' => CTok::Close,
                        b',' => CTok::Comma,
                        _ => CTok::Eq,
                    });
                }
                b'"' => {
                    let start = self.at + 1;
                    let mut end = start;
                    while end < bytes.len() && bytes[end] != b'"' {
                        end += if bytes[end] == b'\\' { 2 } else { 1 };
                    }
                    if end >= bytes.len() {
                        self.bad = true;
                        return None;
                    }
                    self.at = end + 1;
                    return Some(CTok::Str(&text[start..end]));
                }
                _ if c == b'_' || c.is_ascii_alphanumeric() => {
                    let start = self.at;
                    while self.at < bytes.len() && (bytes[self.at] == b'_' || bytes[self.at].is_ascii_alphanumeric()) {
                        self.at += 1;
                    }
                    return Some(CTok::Ident(&text[start..self.at]));
                }
                _ => {
                    self.bad = true;
                    return None;
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Attributes.

/// What one item's attributes say, a true `cfg_attr`'s applied.
#[derive(Clone, Debug)]
pub struct Attrs<'t, 'a> {
    pub cfg_ok: bool,
    pub cfg_attrs: u32,
    /// `cfg` and `cfg_attr` predicates that did not read ([`Cfg::eval`]): each taken as false.
    pub cfg_unread: u32,
    pub path: Option<&'t str>,
    pub derives: AVec<'a, &'t str>,
    pub macro_export: bool,
    pub local_inner: bool,
    pub macro_use: bool,
    pub lang: Option<&'t str>,
    pub builtin: bool,
    pub builtin_name: Option<&'t str>,
    pub prelude_import: bool,
    pub no_implicit_prelude: bool,
    pub no_std: bool,
    pub no_core: bool,
    /// `#![blaze::arbitrary_ints]` (docs/v2/plan/design/arbitrary-ints.md).
    pub arbitrary_ints: bool,
    pub tools: AVec<'a, &'t str>,
    /// Attribute names that are neither built in nor a tool's: attribute macros, not lowered.
    pub others: AVec<'a, &'t str>,
    /// The outermost attribute macro, as written (`bstd::main`): the item is its input, and
    /// declares nothing until the call expands.
    pub attr_macro: Option<&'t str>,
    /// `#[proc_macro]`, `#[proc_macro_attribute]` or `#[proc_macro_derive(Name, attributes(..))]`
    /// on a function: the macro it exports (the last one written, as rustc refuses two).
    pub proc_macro: Option<ProcAttr<'t>>,
}

/// The macro a proc-macro crate's function exports (the Reference, "Procedural Macros"): a
/// function-like or attribute macro is named by the function, a derive by its attribute's first
/// argument; a derive's helper attributes are the names in `attributes(..)`, as written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcAttr<'t> {
    pub kind: ProcKind,
    /// The derive's name; `None` for the other kinds (the function's name).
    pub derive: Option<&'t str>,
    /// What is inside `attributes(..)`, comma-separated; empty when there is none.
    pub helpers: &'t str,
}

/// One macro a proc-macro crate's built module declares to the host (blaze-proc-macro's
/// `Exported`, borrowed): the truth about what the crate exports. Its text cannot always say:
/// a derive declared through another macro (`synstructure::decl_derive!`) has no
/// `#[proc_macro_derive]` written anywhere. `helpers` as [`ProcAttr::helpers`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcExport<'t> {
    pub kind: ProcKind,
    pub name: &'t str,
    pub helpers: &'t str,
}

/// Where a crate's procedural macros are declared from.
#[derive(Clone, Copy, Debug)]
pub enum ProcDecls<'c> {
    /// From the `#[proc_macro*]` attributes of its text: no module is held for it.
    Text,
    /// From its module's list: this fragment declares `here` (the crate root's fragment the
    /// whole list, every other fragment none), and no text attribute declares a macro.
    Module { here: &'c [ProcExport<'c>] },
}

impl<'t> ProcAttr<'t> {
    /// Reads `proc_macro`, `proc_macro_attribute` or `proc_macro_derive(..)` (an attribute's
    /// content); `None` for any other attribute or a derive without a name.
    pub fn read(content: &'t str) -> Option<ProcAttr<'t>> {
        let kind = match head(content) {
            "proc_macro" => ProcKind::FnLike,
            "proc_macro_attribute" => ProcKind::Attr,
            "proc_macro_derive" => ProcKind::Derive,
            _ => return None,
        };
        if kind != ProcKind::Derive {
            return Some(ProcAttr { kind, derive: None, helpers: "" });
        }
        let mut parts = SplitTop::new(args(content));
        let name = unraw(parts.next()?.trim());
        if name.is_empty() {
            return None;
        }
        let helpers = parts.map(str::trim).find(|part| head(part) == "attributes").map_or("", args);
        Some(ProcAttr { kind, derive: Some(name), helpers })
    }

    /// The helper attributes' names, in the order written.
    pub fn helper_names(helpers: &'t str) -> impl Iterator<Item = &'t str> {
        SplitTop::new(helpers).map(|it| unraw(it.trim())).filter(|it| !it.is_empty())
    }
}

impl<'a> Attrs<'_, 'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        Attrs {
            cfg_ok: true,
            cfg_attrs: 0,
            cfg_unread: 0,
            path: None,
            derives: AVec::new_in(alloc),
            macro_export: false,
            local_inner: false,
            macro_use: false,
            lang: None,
            builtin: false,
            builtin_name: None,
            prelude_import: false,
            no_implicit_prelude: false,
            no_std: false,
            no_core: false,
            arbitrary_ints: false,
            tools: AVec::new_in(alloc),
            others: AVec::new_in(alloc),
            attr_macro: None,
            proc_macro: None,
        }
    }
}

/// The attribute names the toolchain reads itself.
pub const BUILTIN_ATTRS: &[&str] = &[
    "allow", "automatically_derived", "cfg", "cfg_attr", "cfg_eval", "cold", "collapse_debuginfo",
    "coverage", "crate_name", "crate_type", "debugger_visualizer", "default", "deny", "deprecated",
    "derive", "diagnostic", "doc", "expect", "export_name", "feature", "forbid", "global_allocator",
    "ignore", "inline", "instruction_set", "lang", "link", "link_name", "link_ordinal",
    "link_section", "linkage", "macro_escape", "macro_export", "macro_use", "must_not_suspend",
    "must_use", "naked", "no_builtins", "no_core", "no_implicit_prelude", "no_link", "no_main",
    "no_mangle", "no_std", "non_exhaustive", "optimize", "panic_handler", "path", "prelude_import",
    "proc_macro", "proc_macro_attribute", "proc_macro_derive", "recursion_limit", "register_tool",
    "repr", "should_panic", "stable", "target_feature", "test", "thread_local", "track_caller",
    "type_length_limit", "unsafe", "unstable", "used", "warn", "windows_subsystem", "const_trait",
    "derive_const", "fundamental", "marker", "may_dangle", "ffi_const", "ffi_pure",
    "omit_gdb_pretty_printer_section", "allow_internal_unstable", "allow_internal_unsafe",
    "pointee", "unstable_feature_bound", "align", "sanitize", "loop_match", "const_continue",
    "patchable_function_entry", "define_opaque", "type_const", "coroutine", "custom_mir",
    "do_not_recommend", "on_unimplemented", "rustfmt", "clippy",
];

pub fn is_builtin_attr(name: &str) -> bool {
    name.starts_with("rustc_") || name.contains("::") || BUILTIN_ATTRS.contains(&name)
}

/// The attribute's content: `#[a(b)]` and `#![a(b)]` give `a(b)`.
pub fn strip_attr(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.strip_prefix('#').unwrap_or(s).trim_start();
    let s = s.strip_prefix('!').unwrap_or(s).trim_start();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);
    s.trim()
}

/// The path an attribute's content starts with.
pub fn head(content: &str) -> &str {
    // An expansion's text holds spaces between its tokens (`:: core :: prelude :: v1 ::
    // derive (..)`): a space continues the path only beside a `::`.
    let b = content.as_bytes();
    let path_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b':';
    let (mut i, mut end) = (0, 0);
    while i < b.len() {
        if path_byte(b[i]) {
            i += 1;
            end = i;
            continue;
        }
        if !b[i].is_ascii_whitespace() {
            break;
        }
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let after_colons = end > 0 && b[end - 1] == b':';
        let before_colons = j < b.len() && b[j] == b':';
        if !(j < b.len() && path_byte(b[j]) && (after_colons || before_colons)) {
            break;
        }
        i = j;
    }
    &content[..end]
}

/// A built-in attribute named by its path in a standard library's prelude
/// (`::core::prelude::v1::derive`, `std::prelude::rust_2021::test`): the Reference, "Attributes",
/// "Built-in attributes index", and "Preludes", "Standard library prelude": the built-in
/// attributes are items of `core::prelude::v1` and each edition's module beside it, so that path
/// names the same attribute as its bare name. Any other path is returned as it is.
pub fn prelude_attr(path: &str) -> &str {
    let mut segments = path.split("::").map(str::trim);
    let lead = segments.next();
    let lead = if lead == Some("") { segments.next() } else { lead };
    let (Some(krate), Some("prelude"), Some(_), Some(last), None) = (lead, segments.next(), segments.next(), segments.next(), segments.next()) else {
        return path;
    };
    if matches!(krate, "core" | "std" | "alloc") { last } else { path }
}

/// What is inside an attribute's outer parentheses, or nothing.
pub fn args(content: &str) -> &str {
    match (content.find('('), content.rfind(')')) {
        (Some(open), Some(close)) if close > open => &content[open + 1..close],
        _ => "",
    }
}

/// The first string literal's inside.
pub fn string_value(content: &str) -> Option<&str> {
    let open = content.find('"')?;
    let rest = &content[open + 1..];
    let close = rest.find('"')?;
    Some(&rest[..close])
}

/// `text` split at the commas outside brackets and strings; a last part only when it is not
/// blank.
#[derive(Clone, Debug)]
pub struct SplitTop<'t> {
    text: &'t str,
    at: usize,
    start: usize,
    depth: i32,
    in_str: bool,
}

impl<'t> SplitTop<'t> {
    pub fn new(text: &'t str) -> SplitTop<'t> {
        SplitTop { text, at: 0, start: 0, depth: 0, in_str: false }
    }
}

impl<'t> Iterator for SplitTop<'t> {
    type Item = &'t str;

    fn next(&mut self) -> Option<&'t str> {
        let text = self.text;
        let bytes = text.as_bytes();
        while self.at < bytes.len() {
            let at = self.at;
            let c = bytes[at];
            self.at += 1;
            if self.in_str {
                if c == b'\\' {
                    self.at += 1;
                } else if c == b'"' {
                    self.in_str = false;
                }
                continue;
            }
            match c {
                b'"' => self.in_str = true,
                b'(' | b'[' | b'{' | b'<' => self.depth += 1,
                b')' | b']' | b'}' => self.depth -= 1,
                b'>' if at == 0 || bytes[at - 1] != b'-' => self.depth -= 1,
                b',' if self.depth == 0 => {
                    let part = &text[self.start..at];
                    self.start = at + 1;
                    return Some(part);
                }
                _ => {}
            }
        }
        if self.start < text.len() {
            let part = &text[self.start..];
            self.start = text.len();
            if !part.trim().is_empty() {
                return Some(part);
            }
        }
        None
    }
}

/// A name without its `r#`.
pub fn unraw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

/// What `item`'s attributes say under `cfg` (`text` is its file's or expansion's).
pub fn item_attrs<'t, 'a>(cfg: &Cfg<'_, '_>, outline: &Outline<'_>, item: &Item, text: &'t str, alloc: Alloc<'a>) -> Attrs<'t, 'a> {
    let mut attrs = Attrs::new_in(alloc);
    let mut stack = AVec::new_in(alloc);
    for attr in outline.attrs(item) {
        if attr.kind == AttrKind::Doc {
            continue;
        }
        cfg.attr(&mut attrs, strip_attr(item.text(text, attr.range)), &mut stack);
    }
    attrs
}

/// What a file's own inner attributes say under `cfg`.
pub fn file_attrs<'t, 'a>(cfg: &Cfg<'_, '_>, outline: &Outline<'_>, text: &'t str, alloc: Alloc<'a>) -> Attrs<'t, 'a> {
    let mut attrs = Attrs::new_in(alloc);
    let mut stack = AVec::new_in(alloc);
    for attr in outline.file_attrs() {
        if attr.kind == AttrKind::Doc {
            continue;
        }
        cfg.attr(&mut attrs, strip_attr(attr.range.of(text)), &mut stack);
    }
    attrs
}

// ---------------------------------------------------------------------------------------------
// Paths and `use` trees.

#[derive(Clone, Copy, PartialEq, Debug)]
enum PTok<'t> {
    Ident(&'t str),
    Colons,
    Open,
    Close,
    Comma,
    Star,
}

/// A path's tokens, one at a time: `None` at the end, `Some(Err(()))` at a byte it does not
/// know.
struct PLex<'t> {
    text: &'t str,
    at: usize,
}

impl<'t> Iterator for PLex<'t> {
    type Item = Result<PTok<'t>, ()>;

    fn next(&mut self) -> Option<Result<PTok<'t>, ()>> {
        let text = self.text;
        let bytes = text.as_bytes();
        while self.at < bytes.len() {
            let at = self.at;
            let c = bytes[at];
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.at += 1,
                b'/' if bytes.get(at + 1) == Some(&b'/') => {
                    while self.at < bytes.len() && bytes[self.at] != b'\n' {
                        self.at += 1;
                    }
                }
                b'/' if bytes.get(at + 1) == Some(&b'*') => {
                    let Some(end) = text[at + 2..].find("*/") else {
                        self.at = bytes.len();
                        return Some(Err(()));
                    };
                    self.at += end + 4;
                }
                b':' if bytes.get(at + 1) == Some(&b':') => {
                    self.at += 2;
                    return Some(Ok(PTok::Colons));
                }
                b'{' => {
                    self.at += 1;
                    return Some(Ok(PTok::Open));
                }
                b'}' => {
                    self.at += 1;
                    return Some(Ok(PTok::Close));
                }
                b',' => {
                    self.at += 1;
                    return Some(Ok(PTok::Comma));
                }
                b'*' => {
                    self.at += 1;
                    return Some(Ok(PTok::Star));
                }
                _ if c == b'$' || c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 => {
                    self.at += 1;
                    if c == b'r' && bytes.get(self.at) == Some(&b'#') {
                        self.at += 1;
                    }
                    while self.at < bytes.len()
                        && (bytes[self.at] == b'_' || bytes[self.at].is_ascii_alphanumeric() || bytes[self.at] >= 0x80)
                    {
                        self.at += 1;
                    }
                    return Some(Ok(PTok::Ident(unraw(&text[at..self.at]))));
                }
                _ => {
                    self.at = bytes.len();
                    return Some(Err(()));
                }
            }
        }
        None
    }
}

fn path_tokens<'t, 'a>(text: &'t str, alloc: Alloc<'a>) -> Option<AVec<'a, PTok<'t>>> {
    let mut out = AVec::new_in(alloc);
    for tok in (PLex { text, at: 0 }) {
        out.push(tok.ok()?);
    }
    Some(out)
}

/// How an expansion's text writes a `$crate` of another crate than the one it expands in:
/// `__dollar_crate_` and the crate key's 64 hex digits: one identifier to the outline's parser
/// and the path reader, read back as `$crate` of that crate.
pub const DOLLAR_CRATE: &str = "__dollar_crate_";

/// [`DOLLAR_CRATE`] of a crate, as it is written.
#[derive(Clone, Copy, Debug)]
pub struct DollarCrate(pub CrateItemKey);

impl fmt::Display for DollarCrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(DOLLAR_CRATE)?;
        self.0.0.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The crate a [`DOLLAR_CRATE`] identifier names.
pub fn dollar_crate_of(text: &str) -> Option<CrateItemKey> {
    let hex = text.strip_prefix(DOLLAR_CRATE)?;
    if hex.len() != 64 {
        return None;
    }
    let mut id = [0u8; 32];
    for (at, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(CrateItemKey(ContentId(id)))
}

/// The path of `segments` as written (`abs` for a leading `::`) in the crate `krate`.
pub fn to_path<'t, 'a>(abs: bool, segments: &[&'t str], krate: CrateItemKey, alloc: Alloc<'a>) -> Path<'a, &'t str> {
    let own = |it: &[&'t str]| AVec::from_slice_in(it, alloc);
    if abs {
        return Path { kind: PathKind::Abs, segments: own(segments) };
    }
    match segments.first().copied() {
        Some("crate") => Path { kind: PathKind::Crate, segments: own(&segments[1..]) },
        Some("$crate") => Path { kind: PathKind::DollarCrate(krate), segments: own(&segments[1..]) },
        Some(first) if first.starts_with(DOLLAR_CRATE) => match dollar_crate_of(first) {
            Some(other) => Path { kind: PathKind::DollarCrate(other), segments: own(&segments[1..]) },
            None => Path { kind: PathKind::Plain, segments: own(segments) },
        },
        Some("self") => Path { kind: PathKind::SelfMod, segments: own(&segments[1..]) },
        Some("super") => {
            let n = segments.iter().take_while(|it| **it == "super").count();
            Path { kind: PathKind::Super(u8::try_from(n).unwrap_or(u8::MAX)), segments: own(&segments[n..]) }
        }
        _ => Path { kind: PathKind::Plain, segments: own(segments) },
    }
}

/// A simple path (`a::b`, `crate::a`, `$crate::a`, `::a`); `None` for anything else.
pub fn parse_path<'t, 'a>(text: &'t str, krate: CrateItemKey, alloc: Alloc<'a>) -> Option<Path<'a, &'t str>> {
    let mut tokens = (PLex { text, at: 0 }).peekable();
    let mut abs = false;
    let mut segments = AVec::new_in(alloc);
    if tokens.peek() == Some(&Ok(PTok::Colons)) {
        abs = true;
        tokens.next();
    }
    loop {
        let Some(Ok(PTok::Ident(name))) = tokens.next() else { return None };
        segments.push(name);
        match tokens.next() {
            None => break,
            Some(Ok(PTok::Colons)) => {}
            Some(_) => return None,
        }
    }
    Some(to_path(abs, &segments, krate, alloc))
}

/// One leaf of a `use` tree.
#[derive(Clone, Debug)]
pub struct UseLeaf<'t, 'a> {
    /// Written with a leading `::`.
    pub abs: bool,
    pub segments: AVec<'a, &'t str>,
    pub form: UseForm<&'t str>,
}

/// The leaves of a `use` tree, in the order written; `None` when it does not read.
pub fn use_leaves<'t, 'a>(text: &'t str, alloc: Alloc<'a>) -> Option<AVec<'a, UseLeaf<'t, 'a>>> {
    /// One open `{ .. }`: the length of the path before it and whether that path is absolute.
    #[derive(Clone, Copy)]
    struct Group {
        base: usize,
        abs: bool,
    }
    let t = path_tokens(text, alloc)?;
    let mut out = AVec::new_in(alloc);
    let mut segments: AVec<'a, &'t str> = AVec::new_in(alloc);
    let mut groups: AVec<'a, Group> = AVec::new_in(alloc);
    let mut at = 0;
    'tree: loop {
        // One tree starts at `at`, under the innermost group (or at the top).
        let (base, mut abs) = groups.last().map_or((0, false), |g| (g.base, g.abs));
        segments.truncate(base);
        if base == 0 && t.get(at) == Some(&PTok::Colons) {
            abs = true;
            at += 1;
        }
        // Whether the tree ends in a named leaf (not a glob or an empty group).
        let mut named = true;
        loop {
            match t.get(at).copied() {
                Some(PTok::Ident(name)) => {
                    segments.push(name);
                    at += 1;
                    if t.get(at) == Some(&PTok::Colons) {
                        at += 1;
                        continue;
                    }
                    break;
                }
                Some(PTok::Open) => {
                    at += 1;
                    if t.get(at) == Some(&PTok::Close) {
                        at += 1;
                        named = false;
                        break;
                    }
                    groups.push(Group { base: segments.len(), abs });
                    continue 'tree;
                }
                Some(PTok::Star) => {
                    at += 1;
                    out.push(UseLeaf { abs, segments: AVec::from_slice_in(&segments, alloc), form: UseForm::Glob });
                    named = false;
                    break;
                }
                _ => return None,
            }
        }
        if named {
            let mut rename = Rename::Same;
            if t.get(at) == Some(&PTok::Ident("as")) {
                at += 1;
                rename = match t.get(at).copied() {
                    Some(PTok::Ident("_")) => Rename::Underscore,
                    Some(PTok::Ident(name)) => Rename::To(name),
                    _ => return None,
                };
                at += 1;
            }
            let mut written = AVec::from_slice_in(&segments, alloc);
            let mut types_only = false;
            if written.len() > 1 && written.last() == Some(&"self") {
                written.pop();
                types_only = true;
            }
            out.push(UseLeaf { abs, segments: written, form: UseForm::Named { rename, types_only } });
        }
        // A tree is read: close the groups it ends.
        loop {
            if groups.is_empty() {
                return (at == t.len()).then_some(out);
            }
            match t.get(at) {
                Some(PTok::Comma) => {
                    at += 1;
                    if t.get(at) != Some(&PTok::Close) {
                        continue 'tree;
                    }
                    at += 1;
                }
                Some(PTok::Close) => at += 1,
                _ => return None,
            }
            groups.pop();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Places.

/// A place in a source file: the file's index and a byte offset. Turned into `file:line` only
/// when printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub file: u32,
    pub offset: u32,
}

/// Where a fragment's text is: a source file, or an expansion (whose places are its call's).
#[derive(Clone, Copy, Debug)]
pub enum Where {
    File(u32),
    Expansion(Loc),
}

// ---------------------------------------------------------------------------------------------
// The module tree.

/// A crate's files as the walk reads them; the caller has read them.
pub trait Files<'t> {
    /// The file at `path` (absolute, `/`-separated, without `.` or `..`), if the crate has it.
    fn find(&self, path: &str) -> Option<u32>;
    /// The file's text.
    fn text(&self, file: u32) -> &'t str;
    /// The file's outline.
    fn outline(&self, file: u32) -> &Outline<'_>;
}

/// Whether a module is one of the tree's.
pub trait Walked {
    fn walked(&self, module: ItemKey) -> bool;
}

/// One module the walk found.
#[derive(Debug)]
pub struct Module<'t, 'a> {
    pub key: ItemKey,
    /// Its parent's index in [`Tree::modules`].
    pub parent: Option<usize>,
    pub name: Option<&'t str>,
    pub file: u32,
    /// The inline `mod` item it is, in `file`; `None` for a file's own items.
    pub item: Option<ItemIndex>,
    /// Where its `mod x;` children's files are.
    pub dir: AString<'a>,
    /// What its children's `#[path]` is relative to.
    pub path_dir: AString<'a>,
    pub no_implicit_prelude: bool,
    /// `krate::a::b`.
    pub path: AString<'a>,
}

/// What the walk could not follow.
#[derive(Debug)]
pub enum WalkNote<'t, 'a> {
    /// Both `x.rs` and `x/mod.rs` exist: `x.rs` taken.
    BothFiles { at: Loc },
    /// Neither `x.rs` nor `x/mod.rs` exists.
    NeitherFile { at: Loc, name: &'t str },
    /// A `#[path]` names a file the crate does not have.
    PathFileNotFound { at: Loc, path: AString<'a> },
    /// More than [`MOST_MODULES`] modules: the rest are not walked.
    TooManyModules { at: Loc },
}

impl fmt::Display for WalkNote<'_, '_> {
    /// The kind of note, as a failure's kind is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WalkNote::BothFiles { .. } => "module: both x.rs and x/mod.rs exist (x.rs taken)",
            WalkNote::NeitherFile { .. } => "module: neither x.rs nor x/mod.rs exists",
            WalkNote::PathFileNotFound { .. } => "module: #[path] file not found",
            WalkNote::TooManyModules { .. } => "module: more modules than the walk takes",
        })
    }
}

/// The modules of a crate, root first, each after its parent.
#[derive(Debug)]
pub struct Tree<'t, 'a> {
    pub modules: AVec<'a, Module<'t, 'a>>,
    /// Each module's index in `modules`.
    pub by_key: HashMap<ItemKey, usize, Alloc<'a>>,
    pub notes: AVec<'a, WalkNote<'t, 'a>>,
    /// Files a `mod x;` may be that the crate does not have: a caller that reads them and walks
    /// again finds the modules they are.
    pub missing: AVec<'a, AString<'a>>,
}

impl Walked for Tree<'_, '_> {
    fn walked(&self, module: ItemKey) -> bool {
        self.by_key.contains_key(&module)
    }
}

/// The most modules one walk makes.
pub const MOST_MODULES: usize = 1_000_000;

/// `part` joined onto the path `out`, `.` and `..` taken out by text alone (a part starting
/// with `/` starts again from the root).
fn join_into(out: &mut AString<'_>, part: &str) {
    if part.starts_with('/') {
        out.clear();
        out.push('/');
    }
    for piece in part.split('/') {
        match piece {
            "" | "." => {}
            ".." => pop_dir(out),
            piece => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(piece);
            }
        }
    }
}

/// `out` without its last component (the root stays).
pub(crate) fn pop_dir(out: &mut AString<'_>) {
    match out.rfind('/') {
        Some(0) => out.truncate(1),
        Some(at) => out.truncate(at),
        None => out.clear(),
    }
}

pub(crate) fn joined<'a>(dir: &str, parts: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    join_into(&mut out, dir);
    for part in parts {
        join_into(&mut out, part);
    }
    out
}

fn copied<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    AString::from_str_in(text, alloc)
}

/// The module tree of the crate `krate` (named `name`) whose root file is `root`, in the
/// directory `root_dir`: each `mod` item that is on under `cfg`, inline or in its file.
pub fn walk<'t, 'a, F: Files<'t> + ?Sized>(
    files: &F,
    cfg: &Cfg<'_, '_>,
    krate: CrateItemKey,
    name: &str,
    root: u32,
    root_dir: &str,
    alloc: Alloc<'a>,
) -> Tree<'t, 'a> {
    let mut tree = Tree {
        modules: AVec::new_in(alloc),
        by_key: HashMap::new_in(alloc),
        notes: AVec::new_in(alloc),
        missing: AVec::new_in(alloc),
    };
    let root_attrs = file_attrs(cfg, files.outline(root), files.text(root), alloc);
    let root_dir = joined(root_dir, &[], alloc);
    tree.modules.push(Module {
        key: root_key(krate),
        parent: None,
        name: None,
        file: root,
        item: None,
        dir: root_dir.clone(),
        path_dir: root_dir,
        no_implicit_prelude: root_attrs.no_implicit_prelude,
        path: copied(name, alloc),
    });
    let mut at = 0;
    while at < tree.modules.len() {
        let m = &tree.modules[at];
        let (file, item, key) = (m.file, m.item, m.key);
        let (dir, path_dir, mpath) = (m.dir.clone(), m.path_dir.clone(), m.path.clone());
        let o = files.outline(file);
        let text = files.text(file);
        for (idx, child) in o.children(item) {
            let ItemDetail::Mod { inline, .. } = child.detail else { continue };
            let attrs = item_attrs(cfg, o, child, text, alloc);
            if !attrs.cfg_ok {
                continue;
            }
            let Some(name) = child.name.map(|r| unraw(child.text(text, r))) else { continue };
            let loc = Loc { file, offset: child.range.start };
            if tree.modules.len() >= MOST_MODULES {
                tree.notes.push(WalkNote::TooManyModules { at: loc });
                break;
            }
            let ckey = key_of(krate, key, "mod", name, 0);
            let mut cpath = mpath.clone();
            cpath.push_str("::");
            cpath.push_str(name);
            if inline {
                // An inline module's `#[path]` names its directory against the directory a
                // `#[path]` of this module resolves against (`path_dir`: a non-mod-rs file's own
                // folder, without the file's module name); without one, the directory is the
                // module's own (`dir`) plus its name.
                let d = match attrs.path {
                    Some(p) => joined(&path_dir, &[p], alloc),
                    None => joined(&dir, &[name], alloc),
                };
                tree.modules.push(Module {
                    key: ckey,
                    parent: Some(at),
                    name: Some(name),
                    file,
                    item: Some(idx),
                    dir: d.clone(),
                    path_dir: d,
                    no_implicit_prelude: attrs.no_implicit_prelude,
                    path: cpath,
                });
                continue;
            }
            let (target, cdir, cpdir) = if let Some(p) = attrs.path {
                let target = joined(&path_dir, &[p], alloc);
                let mut parent = target.clone();
                pop_dir(&mut parent);
                (target, parent.clone(), parent)
            } else {
                let mut file_name = AString::new_in(alloc);
                file_name.push_str(name);
                file_name.push_str(".rs");
                let a = joined(&dir, &[&file_name], alloc);
                let b = joined(&dir, &[name, "mod.rs"], alloc);
                let sub = joined(&dir, &[name], alloc);
                match (files.find(&a).is_some(), files.find(&b).is_some()) {
                    (true, true) => {
                        tree.notes.push(WalkNote::BothFiles { at: loc });
                        (a, sub, dir.clone())
                    }
                    (true, false) => {
                        tree.missing.push(b);
                        (a, sub, dir.clone())
                    }
                    (false, true) => {
                        tree.missing.push(a);
                        (b, sub.clone(), sub)
                    }
                    (false, false) => {
                        tree.missing.push(a);
                        tree.missing.push(b);
                        tree.notes.push(WalkNote::NeitherFile { at: loc, name });
                        continue;
                    }
                }
            };
            let Some(cfile) = files.find(&target) else {
                tree.missing.push(target.clone());
                tree.notes.push(WalkNote::PathFileNotFound { at: loc, path: target });
                continue;
            };
            let fattrs = file_attrs(cfg, files.outline(cfile), files.text(cfile), alloc);
            if !fattrs.cfg_ok {
                continue;
            }
            tree.modules.push(Module {
                key: ckey,
                parent: Some(at),
                name: Some(name),
                file: cfile,
                item: None,
                dir: cdir,
                path_dir: cpdir,
                no_implicit_prelude: attrs.no_implicit_prelude || fattrs.no_implicit_prelude,
                path: cpath,
            });
        }
        at += 1;
    }
    for (i, m) in tree.modules.iter().enumerate() {
        tree.by_key.insert(m.key, i);
    }
    tree
}

// ---------------------------------------------------------------------------------------------
// Lowering one fragment.

/// A macro's definition as written.
#[derive(Clone, Copy, Debug)]
pub enum MacroBody<'t> {
    Rules(&'t str),
    Macro2 { params: Option<&'t str>, body: &'t str },
    BuiltinFn(&'t str),
    /// A builtin derive or attribute (`#[rustc_builtin_macro] macro Clone`): not run here.
    BuiltinOther(CallShape),
    /// A proc macro a proc-macro crate exports: run by the crate's module, never here.
    /// `helpers` as [`ProcAttr::helpers`].
    Proc { kind: ProcKind, helpers: &'t str },
}

#[derive(Clone, Copy, Debug)]
pub struct MacroSrc<'t> {
    pub name: &'t str,
    pub body: MacroBody<'t>,
    pub module: ItemKey,
    pub export: bool,
    pub local_inner: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CallSrc<'t> {
    pub module: ItemKey,
    /// The trait or impl whose body the call stands in (an associated-item position): its
    /// expansion's items are that owner's members ([`Fragment::members_of`]); `None` in a module.
    pub owner: Option<ItemKey>,
    pub name: &'t str,
    pub input: &'t str,
    pub kind: CallShape,
    pub loc: Loc,
    pub depth: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct ImplSrc<'t> {
    pub module: ItemKey,
    pub key: ItemKey,
    pub header: &'t str,
    pub loc: Loc,
}

#[derive(Clone, Copy, Debug)]
pub struct LangSrc<'t> {
    pub name: &'t str,
    pub key: ItemKey,
    pub loc: Loc,
}

/// A `mod` item written by a macro, or inside a module one wrote.
#[derive(Clone, Copy, Debug)]
pub struct ModMade<'t> {
    pub key: ItemKey,
    pub name: &'t str,
    /// The item in the outline it was lowered from.
    pub item: ItemIndex,
    pub inline: bool,
    pub path_attr: Option<&'t str>,
    pub no_implicit_prelude: bool,
    pub loc: Loc,
}

/// An item's whole text and the module its names are read in.
#[derive(Clone, Copy, Debug)]
pub struct ItemText<'t> {
    pub key: ItemKey,
    pub module: ItemKey,
    /// Its entry in the fragment's outline: its kind and attributes are read there.
    pub item: ItemIndex,
    pub name: Option<&'t str>,
    pub impl_header: Option<&'t str>,
    pub text: &'t str,
}

/// A member of a trait or an impl: an associated fn, const or type (or a macro call).
#[derive(Clone, Copy, Debug)]
pub struct Member<'t> {
    pub key: ItemKey,
    pub name: Option<&'t str>,
    /// Its entry in the outline of the text its owner is in.
    pub item: ItemIndex,
    pub detail: ItemDetail,
    /// Its whole text, attributes included.
    pub text: &'t str,
    /// The call whose expansion it is in (its outline is that expansion's:
    /// `CrateNames::expansions`); `None` when written in its owner's body.
    pub written_by: Option<ItemKey>,
}

/// A `#[prelude_import] use path::*;`.
#[derive(Clone, Debug)]
pub struct PreludeImport<'t, 'a> {
    pub abs: bool,
    pub segments: AVec<'a, &'t str>,
    pub loc: Loc,
}

/// What the lowering of an item could not read, one kind per reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LowerNoteKind<'t> {
    /// An attribute that is neither built in nor a tool's: an attribute macro.
    AttrMacroNotLowered { attr: &'t str },
    DerivePathNotRead,
    UseWithoutTree,
    UseTreeNotRead,
    PreludeImportNotOneGlob,
    MacroRulesWithoutBody,
    MacroWithoutBody,
    MacroCallWithoutPathOrInput,
    MacroCallPathNotRead,
    ItemNotRead,
    /// An item of a syntax kind the outline does not take apart.
    UnhandledItem { syntax: u16 },
}

impl fmt::Display for LowerNoteKind<'_> {
    /// The kind of note, as a failure's kind is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LowerNoteKind::AttrMacroNotLowered { attr } => write!(f, "attribute macro not lowered: #[{attr}]"),
            LowerNoteKind::DerivePathNotRead => f.write_str("derive path not read"),
            LowerNoteKind::UseWithoutTree => f.write_str("use without a tree"),
            LowerNoteKind::UseTreeNotRead => f.write_str("use tree not read"),
            LowerNoteKind::PreludeImportNotOneGlob => f.write_str("#[prelude_import] that is not one glob"),
            LowerNoteKind::MacroRulesWithoutBody => f.write_str("macro_rules! without a body"),
            LowerNoteKind::MacroWithoutBody => f.write_str("macro without a body"),
            LowerNoteKind::MacroCallWithoutPathOrInput => f.write_str("macro call without a path or an input"),
            LowerNoteKind::MacroCallPathNotRead => f.write_str("macro call path not read"),
            LowerNoteKind::ItemNotRead => f.write_str("an item the outline could not read"),
            LowerNoteKind::UnhandledItem { syntax } => write!(f, "unhandled item form: {:?}", syntax_kind(*syntax)),
        }
    }
}

/// One thing the lowering could not read.
#[derive(Clone, Copy, Debug)]
pub struct LowerNote<'t> {
    pub kind: LowerNoteKind<'t>,
    /// What it is about as written: the item's name, the path or tree, the item's first line
    /// (empty where there is none).
    pub what: &'t str,
    /// The item's place (its call's, in an expansion).
    pub at: Loc,
    pub in_expansion: bool,
}

/// One fragment lowered: a module's own text, or one call's expansion.
#[derive(Debug)]
pub struct Lowered<'t, 'a> {
    pub decls: DeclsBuilder<'a, &'t str>,
    pub macros: AVec<'a, (ItemKey, MacroSrc<'t>)>,
    pub calls: AVec<'a, (ItemKey, CallSrc<'t>)>,
    pub enums: AVec<'a, (ItemKey, AVec<'a, Variant<&'t str>>)>,
    pub impls: AVec<'a, ImplSrc<'t>>,
    pub langs: AVec<'a, LangSrc<'t>>,
    /// Where each of `decls.uses` is written, by index.
    pub use_locs: AVec<'a, Loc>,
    pub prelude: Option<PreludeImport<'t, 'a>>,
    /// The crates of `#[macro_use] extern crate x;` items, by the name written.
    pub macro_use: AVec<'a, &'t str>,
    /// Each item's text (an impl's too).
    pub texts: AVec<'a, ItemText<'t>>,
    /// The `mod` items of a module a macro wrote (or of one inside it): modules the walk did
    /// not see, for the driver to make.
    pub mods_made: AVec<'a, ModMade<'t>>,
    /// Each trait's and impl's members on under the cfg, in declaration order, by its key.
    pub members: AVec<'a, (ItemKey, AVec<'a, Member<'t>>)>,
    /// The tuple and unit structs: each is its own constructor's key.
    pub ctors: AVec<'a, ItemKey>,
    pub notes: AVec<'a, LowerNote<'t>>,
}

impl Lowered<'_, '_> {
    /// The declarations' entries.
    pub fn entries(&self) -> usize {
        let d = &self.decls;
        d.items.len() + d.mods.len() + d.uses.len() + d.extern_crates.len() + d.macro_rules.len() + d.calls.len()
    }
}

/// What every fragment of a crate is lowered under.
pub struct LowerCx<'c, 'p, W: ?Sized> {
    pub krate: CrateItemKey,
    /// The crate's edition: a `macro_rules!` body's arms are lexed in it ([`rules_shapes`]).
    pub edition: Edition,
    pub cfg: &'c Cfg<'c, 'c>,
    /// The modules the walk found.
    pub walked: &'c W,
    /// Where its procedural macros are declared from.
    pub procs: ProcDecls<'p>,
}

impl<W: ?Sized> Clone for LowerCx<'_, '_, W> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<W: ?Sized> Copy for LowerCx<'_, '_, W> {}

/// One fragment's text and place.
#[derive(Clone, Copy)]
pub struct Fragment<'c, 't> {
    /// The module its names are declared in.
    pub module: ItemKey,
    /// Its key: the module's, or the call's.
    pub owner: ItemKey,
    pub text: &'t str,
    pub outline: &'c Outline<'c>,
    /// The entries it is: the outline's own (`None`) or an inline module's.
    pub items: Option<ItemIndex>,
    pub at: Where,
    /// How many expansions deep it is.
    pub depth: u16,
    /// A module the walk did not see (a macro wrote it or its parent): its `mod` items are
    /// made, not looked up in the tree.
    pub made: bool,
    /// An expansion of a call in this trait's or impl's body: its entries are the owner's
    /// members ([`Lower::assoc`]), not a module's items.
    pub members_of: Option<ItemKey>,
}

/// Lowers `fragment`'s items under `cx`.
pub fn lower<'p: 't, 't, 'a, W: Walked + ?Sized>(cx: LowerCx<'_, 'p, W>, fragment: Fragment<'_, 't>, alloc: Alloc<'a>) -> Lowered<'t, 'a> {
    let mut low = Lower {
        krate: cx.krate,
        edition: cx.edition,
        cfg: cx.cfg,
        walked: cx.walked,
        module: fragment.module,
        owner: fragment.owner,
        text: fragment.text,
        outline: fragment.outline,
        at: fragment.at,
        depth: fragment.depth,
        made: fragment.made,
        pos: 0,
        procs: cx.procs,
        alloc,
        stack: AVec::new_in(alloc),
        seen: HashMap::new_in(alloc),
        out: Lowered {
            decls: DeclsBuilder::new_in(alloc),
            macros: AVec::new_in(alloc),
            calls: AVec::new_in(alloc),
            enums: AVec::new_in(alloc),
            impls: AVec::new_in(alloc),
            langs: AVec::new_in(alloc),
            use_locs: AVec::new_in(alloc),
            prelude: None,
            macro_use: AVec::new_in(alloc),
            texts: AVec::new_in(alloc),
            mods_made: AVec::new_in(alloc),
            members: AVec::new_in(alloc),
            ctors: AVec::new_in(alloc),
            notes: AVec::new_in(alloc),
        },
    };
    // The entries left to lower, each a run of siblings (`at..end`): an extern block's items
    // are lowered in its place, before its next sibling.
    let outline = fragment.outline;
    if let Some(owner) = fragment.members_of {
        low.assoc(None, owner);
        return low.out;
    }
    // A macro crate's module list: the root's fragment declares each macro it names.
    if let ProcDecls::Module { here } = low.procs {
        for export in here {
            low.proc_macro(*export);
        }
    }
    let mut runs: AVec<'a, (usize, usize)> = AVec::new_in(alloc);
    runs.push(children_run(outline, fragment.items));
    while let Some(run) = runs.last_mut() {
        if run.0 >= run.1 {
            runs.pop();
            continue;
        }
        let at = run.0;
        let item = &outline.items()[at];
        run.0 = item.end.at();
        let idx = ItemIndex(u32::try_from(at).expect("an outline's entries are counted in u32"));
        if low.item(idx, item) {
            runs.push(children_run(outline, Some(idx)));
        }
    }
    low.out
}

/// The entries directly in `parent` (in the file itself, for `None`), as `at..end`.
fn children_run(outline: &Outline<'_>, parent: Option<ItemIndex>) -> (usize, usize) {
    match parent {
        None => (0, outline.items().len()),
        Some(parent) => (parent.at() + 1, outline.item(parent).end.at()),
    }
}

struct Lower<'c, 't, 'a, W: ?Sized> {
    krate: CrateItemKey,
    edition: Edition,
    cfg: &'c Cfg<'c, 'c>,
    walked: &'c W,
    module: ItemKey,
    owner: ItemKey,
    text: &'t str,
    outline: &'c Outline<'c>,
    at: Where,
    depth: u16,
    made: bool,
    pos: u32,
    procs: ProcDecls<'t>,
    alloc: Alloc<'a>,
    /// `cfg_attr`'s attributes not yet read, for [`Cfg::attr`].
    stack: AVec<'a, &'t str>,
    /// How many entries of each owner, kind and name this fragment has keyed: [`Lower::key`].
    seen: HashMap<(ItemKey, &'t str, &'t str), u32, Alloc<'a>>,
    out: Lowered<'t, 'a>,
}

impl<'t, 'a, W: Walked + ?Sized> Lower<'_, 't, 'a, W> {
    /// The key of `owner`'s next entry of kind `tag` named `name`. Its index counts only the
    /// earlier entries of that owner, kind and name (two `impl`s, two `_` consts, one item
    /// under two `#[cfg]`s): an item added, removed or moved changes no other item's key, so
    /// no record that names one changes with it (rule 71: positions are never in a key).
    fn key(&mut self, owner: ItemKey, tag: &'t str, name: &'t str) -> ItemKey {
        let n = self.seen.entry((owner, tag, name)).or_insert(0);
        let key = key_of(self.krate, owner, tag, name, *n);
        *n += 1;
        key
    }

    fn loc(&self, offset: u32) -> Loc {
        match self.at {
            Where::File(file) => Loc { file, offset },
            Where::Expansion(loc) => loc,
        }
    }

    fn note(&mut self, kind: LowerNoteKind<'t>, offset: u32, what: &'t str) {
        let note = LowerNote { kind, what, at: self.loc(offset), in_expansion: matches!(self.at, Where::Expansion(_)) };
        self.out.notes.push(note);
    }

    fn next_pos(&mut self) -> u32 {
        self.pos += 1;
        self.pos
    }

    fn attrs(&mut self, item: &Item) -> Attrs<'t, 'a> {
        let mut attrs = Attrs::new_in(self.alloc);
        let outline = self.outline;
        for attr in outline.attrs(item) {
            if attr.kind == AttrKind::Doc {
                continue;
            }
            self.cfg.attr(&mut attrs, strip_attr(item.text(self.text, attr.range)), &mut self.stack);
        }
        attrs
    }

    fn vis(&self, item: &Item) -> WrittenVis<Path<'a, &'t str>> {
        let path = |r| {
            parse_path(item.text(self.text, r), self.krate, self.alloc)
                .unwrap_or_else(|| to_path(false, &[item.text(self.text, r)], self.krate, self.alloc))
        };
        match item.visibility {
            WrittenVis::Private => WrittenVis::Private,
            WrittenVis::Pub => WrittenVis::Pub,
            WrittenVis::Crate => WrittenVis::Crate,
            WrittenVis::Super => WrittenVis::Super,
            WrittenVis::SelfOnly => WrittenVis::SelfOnly,
            WrittenVis::In(r) => WrittenVis::In(path(r)),
            WrittenVis::WithoutIn(r) => WrittenVis::WithoutIn(path(r)),
        }
    }

    fn declare(&mut self, name: &'t str, ns: Namespace, key: ItemKey, kind: DefKind, vis: WrittenVis<Path<'a, &'t str>>) {
        self.out.decls.items.push(DeclaredItem { name, ns, def: Def { key, kind }, vis });
    }

    /// The macro the function `function` exports (the Reference, "Procedural Macros"): bound
    /// in the macro namespace of the module the function is in (rustc requires the crate root),
    /// public, so a dependent names it and re-exports it like any macro. A derive and a trait of
    /// one name do not meet: they are in different namespaces. Its own key, so the function stays
    /// a value of its own.
    fn proc_macro(&mut self, proc: ProcExport<'t>) {
        let name = proc.name;
        let shape = match proc.kind {
            ProcKind::FnLike => MacroShapes::FN_LIKE,
            ProcKind::Attr => MacroShapes::ATTR,
            ProcKind::Derive => MacroShapes::DERIVE,
        };
        let key = self.key(self.owner, "proc_macro", name);
        self.declare(name, Namespace::Macros, key, DefKind::Macro(shape), WrittenVis::Pub);
        let src = MacroSrc { name, body: MacroBody::Proc { kind: proc.kind, helpers: proc.helpers }, module: self.module, export: true, local_inner: false };
        self.out.macros.push((key, src));
    }

    /// Lowers one entry; true when its children are to be lowered next, in its place (an
    /// extern block's).
    fn item(&mut self, idx: ItemIndex, item: &Item) -> bool {
        let text = self.text;
        let attrs = self.attrs(item);
        if !attrs.cfg_ok {
            return false;
        }
        let at = item.range.start;
        let name = item.name.map(|r| unraw(item.text(text, r)));
        let vis = self.vis(item);
        let key = self.key(self.owner, kind_tag(item.detail.kind()), name.unwrap_or(""));
        let whole = &text[item.range.start as usize..item.range.end as usize];
        let impl_header = matches!(item.detail, ItemDetail::Impl).then(|| self.impl_header(item));
        self.out.texts.push(ItemText { key, module: self.module, item: idx, name, impl_header, text: whole });
        // A procedural macro's function under another attribute macro (alloy-sol-macro's
        // `#[proc_macro] #[proc_macro_error] pub fn sol`) is declared from its text: the
        // Reference, "Procedural Macros": a proc-macro crate exports only its procedural
        // macros, so the attribute can change the function's body, which no name outside the
        // crate reads, and not what the crate exports. Its call is not made.
        let proc_fn = attrs.proc_macro.is_some() && matches!(item.detail, ItemDetail::Fn { .. });
        if let Some(path) = attrs.attr_macro.filter(|_| !proc_fn) {
            // The item is the call's input: what it declares is the expansion's (its derives
            // and later attribute macros too, lowered from the expansion).
            let call = self.key(self.owner, "attr", path);
            let Some(parsed) = parse_path(path, self.krate, self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: path }, at, name.unwrap_or("_"));
                return false;
            };
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0) });
            let src = CallSrc { module: self.module, owner: None, name: path, input: whole, kind: CallShape::Attr, loc: self.loc(at), depth: self.depth };
            self.out.calls.push((call, src));
            return false;
        }
        for &path in attrs.derives.iter() {
            let call = self.key(self.owner, "derive", path);
            let Some(parsed) = parse_path(path, self.krate, self.alloc) else {
                self.note(LowerNoteKind::DerivePathNotRead, at, path);
                continue;
            };
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Derive, pos, item: Some(idx.0) });
            let src = CallSrc { module: self.module, owner: None, name: path, input: whole, kind: CallShape::Derive, loc: self.loc(at), depth: self.depth };
            self.out.calls.push((call, src));
        }
        if let Some(lang) = attrs.lang {
            let loc = self.loc(at);
            self.out.langs.push(LangSrc { name: lang, key, loc });
        }
        match item.detail {
            ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } => {
                if let Some(name) = name.filter(|it| *it != "_") {
                    // Declared from the text only when no module's list is held.
                    let from_text = matches!(item.detail, ItemDetail::Fn { .. }) && matches!(self.procs, ProcDecls::Text);
                    if let Some(proc) = attrs.proc_macro.filter(|_| from_text) {
                        self.proc_macro(ProcExport { kind: proc.kind, name: proc.derive.unwrap_or(name), helpers: proc.helpers });
                    }
                    self.declare(name, Namespace::Values, key, DefKind::Other, vis);
                }
            }
            ItemDetail::Struct => {
                let Some(name) = name else { return false };
                let shape = item.body.map(|r| item.text(text, r).trim_start().as_bytes().first().copied());
                let value = matches!(shape, None | Some(Some(b'(')));
                if value {
                    // The constructor of a tuple or unit struct is the struct's own key, bound
                    // in the value namespace.
                    self.declare(name, Namespace::Values, key, DefKind::Other, vis.clone());
                    self.out.ctors.push(key);
                }
                self.declare(name, Namespace::Types, key, DefKind::Other, vis);
            }
            ItemDetail::Union | ItemDetail::TypeAlias | ItemDetail::TraitAlias => {
                if let Some(name) = name {
                    self.declare(name, Namespace::Types, key, DefKind::Other, vis);
                }
            }
            ItemDetail::Enum => {
                let Some(name) = name else { return false };
                self.declare(name, Namespace::Types, key, DefKind::Enum, vis);
                let mut variants = AVec::new_in(self.alloc);
                let outline = self.outline;
                for (vidx, v) in outline.children(Some(idx)) {
                    if !matches!(v.detail, ItemDetail::Variant { .. }) {
                        continue;
                    }
                    let vattrs = self.attrs(v);
                    if !vattrs.cfg_ok {
                        continue;
                    }
                    let Some(vname) = v.name.map(|r| unraw(v.text(text, r))) else { continue };
                    let record = v.body.is_some_and(|r| v.text(text, r).trim_start().starts_with('{'));
                    let vkey = self.key(key, "variant", vname);
                    // `#[lang]` on a variant (core's `ControlFlow::{Continue, Break}`, `Option::{Some,
                    // None}`, `Result::{Ok, Err}`, `Poll::{Ready, Pending}`): `?` desugars to them.
                    if let Some(lang) = vattrs.lang {
                        let loc = self.loc(v.range.start);
                        self.out.langs.push(LangSrc { name: lang, key: vkey, loc });
                    }
                    variants.push(Variant { name: vname, key: vkey, record });
                }
                self.out.enums.push((key, variants));
            }
            ItemDetail::Trait => {
                if let Some(name) = name {
                    self.declare(name, Namespace::Types, key, DefKind::Trait, vis);
                }
                self.assoc(Some(idx), key);
            }
            ItemDetail::Impl => {
                let header = self.impl_header(item);
                let loc = self.loc(at);
                self.out.impls.push(ImplSrc { module: self.module, key, header, loc });
                self.assoc(Some(idx), key);
            }
            ItemDetail::Mod { inline, .. } => {
                let Some(name) = name else { return false };
                if self.made || matches!(self.at, Where::Expansion(_)) {
                    // Keyed by the fragment it is written in (the call, or the made module).
                    let pos = self.next_pos();
                    let key = self.key(self.owner, "mod", name);
                    let loc = self.loc(at);
                    self.out.mods_made.push(ModMade {
                        key,
                        name,
                        item: idx,
                        inline,
                        path_attr: attrs.path,
                        no_implicit_prelude: attrs.no_implicit_prelude,
                        loc,
                    });
                    self.out.decls.mods.push(ChildModDecl { name, module: key, vis, macro_use: attrs.macro_use, pos });
                    return false;
                }
                let module = key_of(self.krate, self.module, "mod", name, 0);
                if !self.walked.walked(module) {
                    // Not walked: its file is missing or cfg'd out; the walk noted it.
                    return false;
                }
                let pos = self.next_pos();
                self.out.decls.mods.push(ChildModDecl { name, module, vis, macro_use: attrs.macro_use, pos });
            }
            ItemDetail::Use { tree } => {
                let Some(tree) = tree else {
                    self.note(LowerNoteKind::UseWithoutTree, at, "");
                    return false;
                };
                let tree_text = item.text(text, tree);
                let Some(leaves) = use_leaves(tree_text, self.alloc) else {
                    self.note(LowerNoteKind::UseTreeNotRead, at, tree_text);
                    return false;
                };
                if attrs.prelude_import {
                    match leaves.as_slice() {
                        [UseLeaf { abs, segments, form: UseForm::Glob }] => {
                            let loc = self.loc(at);
                            self.out.prelude = Some(PreludeImport { abs: *abs, segments: segments.clone(), loc });
                        }
                        _ => self.note(LowerNoteKind::PreludeImportNotOneGlob, at, tree_text),
                    }
                    return false;
                }
                for leaf in leaves {
                    let pos = self.next_pos();
                    let path = to_path(leaf.abs, &leaf.segments, self.krate, self.alloc);
                    self.out.decls.uses.push(UseDecl { path, leaf: leaf.form, vis: vis.clone(), pos });
                    let loc = self.loc(at);
                    self.out.use_locs.push(loc);
                }
            }
            ItemDetail::ExternCrate { rename } => {
                let Some(name) = name else { return false };
                let target = if name == "self" { ExternTarget::SelfCrate } else { ExternTarget::Dep(name) };
                let rename = rename.map(|r| unraw(item.text(text, r)));
                let rename = match rename {
                    Rename::To("_") => Rename::Underscore,
                    other => other,
                };
                if attrs.macro_use {
                    if let ExternTarget::Dep(written) = target {
                        self.out.macro_use.push(written);
                    }
                }
                self.out.decls.extern_crates.push(ExternCrateDecl { target, rename, vis });
            }
            ItemDetail::ExternBlock { .. } => return true,
            ItemDetail::MacroRules { exported } => {
                let Some(name) = name else { return false };
                let Some(body) = item.body.map(|r| inside(item.text(text, r))) else {
                    self.note(LowerNoteKind::MacroRulesWithoutBody, at, name);
                    return false;
                };
                let pos = self.next_pos();
                let export = exported || attrs.macro_export;
                let shapes = if attrs.builtin { MacroShapes::FN_LIKE } else { rules_shapes(body, self.edition, self.alloc) };
                self.out.decls.macro_rules.push(MacroRulesDecl { name, def: key, pos, export, shapes });
                let body = if attrs.builtin { MacroBody::BuiltinFn(attrs.builtin_name.unwrap_or(name)) } else { MacroBody::Rules(body) };
                let src = MacroSrc { name, body, module: self.module, export, local_inner: attrs.local_inner };
                self.out.macros.push((key, src));
            }
            ItemDetail::MacroDef => {
                let Some(name) = name else { return false };
                let Some(body_range) = item.body else {
                    self.note(LowerNoteKind::MacroWithoutBody, at, name);
                    return false;
                };
                let (shape, body) = if attrs.builtin {
                    let which = attrs.builtin_name.unwrap_or(name);
                    if BuiltinDerive::ALL.into_iter().any(|it| it.name() == which) {
                        (MacroShapes::DERIVE, MacroBody::BuiltinOther(CallShape::Derive))
                    } else if BuiltinAttr::ALL.into_iter().any(|it| it.name() == which) {
                        (MacroShapes::ATTR, MacroBody::BuiltinOther(CallShape::Attr))
                    } else {
                        (MacroShapes::FN_LIKE, MacroBody::BuiltinFn(which))
                    }
                } else {
                    let params = item.name.map(|r| {
                        let between = Range::new(r.end, body_range.start);
                        item.text(text, between).trim()
                    });
                    let params = params.filter(|it| it.starts_with('(')).map(inside);
                    let body = inside(item.text(text, body_range));
                    // `macro m(..) {..}` is one function-like rule; `macro m {..}` has arms.
                    let shapes = match params {
                        Some(_) => MacroShapes::FN_LIKE,
                        None => rules_shapes(body, self.edition, self.alloc),
                    };
                    (shapes, MacroBody::Macro2 { params, body })
                };
                self.declare(name, Namespace::Macros, key, DefKind::Macro(shape), vis);
                let src = MacroSrc { name, body, module: self.module, export: false, local_inner: false };
                self.out.macros.push((key, src));
            }
            ItemDetail::MacroCall { .. } => self.fn_like_call(item, key, None),
            ItemDetail::Variant { .. } | ItemDetail::Field { .. } => {}
            ItemDetail::Error => {
                let line = first_line(item.text(text, Range::new(0, item.range.end - item.range.start)));
                self.note(LowerNoteKind::ItemNotRead, at, line);
            }
            ItemDetail::Unhandled { syntax } => {
                self.note(LowerNoteKind::UnhandledItem { syntax }, at, name.unwrap_or("_"));
            }
        }
        false
    }

    /// A function-like macro call `item` keyed `call`: its path declared for the solver at this
    /// position and its source kept for the fill. `owner`: the trait or impl whose body it
    /// stands in (its expansion's items are that owner's members), `None` in a module.
    fn fn_like_call(&mut self, item: &Item, call: ItemKey, owner: Option<ItemKey>) {
        let text = self.text;
        let at = item.range.start;
        let ItemDetail::MacroCall { path: Some(path), input: Some(input) } = item.detail else {
            self.note(LowerNoteKind::MacroCallWithoutPathOrInput, at, "");
            return;
        };
        let path_text = item.text(text, path);
        let Some(parsed) = parse_path(path_text, self.krate, self.alloc) else {
            self.note(LowerNoteKind::MacroCallPathNotRead, at, path_text);
            return;
        };
        let pos = self.next_pos();
        self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::FnLike, pos, item: None });
        let src = CallSrc {
            module: self.module,
            owner,
            name: path_text,
            input: inside(item.text(text, input)),
            kind: CallShape::FnLike,
            loc: self.loc(at),
            depth: self.depth,
        };
        self.out.calls.push((call, src));
    }

    /// A trait's or an impl's members that are on under the cfg, in declaration order, and the
    /// lang items among them: the entries in `parent` (written in the owner's body), or, for
    /// `None`, this fragment's top-level entries (an expansion of a call in the owner's body:
    /// the Reference, "Macro invocation": a macro may be invoked where an associated item is
    /// expected, inside `impl` and `trait` blocks, and expands to zero or more associated
    /// items). A member that is itself a macro call is declared like a module's call
    /// ([`Lower::fn_like_call`]) with `parent_key` as its owner: the fill expands it through
    /// the same hook, and what it writes joins `parent_key`'s members, a call it writes in turn
    /// the same way. A named member is keyed by its owner, kind and name wherever it is
    /// written; a call written by an expansion by that expansion's call (its owner's own calls
    /// are counted in the owner's body, an expansion's in its own text).
    fn assoc(&mut self, parent: Option<ItemIndex>, parent_key: ItemKey) {
        let outline = self.outline;
        let text = self.text;
        let written_by = parent.is_none().then_some(self.owner);
        let mut members = AVec::new_in(self.alloc);
        for (idx, item) in outline.children(parent) {
            let attrs = self.attrs(item);
            if !attrs.cfg_ok {
                continue;
            }
            let name = item.name.map(|r| unraw(item.text(text, r)));
            let is_call = matches!(item.detail, ItemDetail::MacroCall { .. });
            let key_owner = match written_by {
                Some(call) if is_call => call,
                _ => parent_key,
            };
            let key = self.key(key_owner, "assoc", name.unwrap_or(""));
            let whole = &text[item.range.start as usize..item.range.end as usize];
            members.push(Member { key, name, item: idx, detail: item.detail, text: whole, written_by });
            if is_call {
                self.fn_like_call(item, key, Some(parent_key));
            }
            if let Some(lang) = attrs.lang {
                let loc = self.loc(item.range.start);
                self.out.langs.push(LangSrc { name: lang, key, loc });
            }
        }
        self.out.members.push((parent_key, members));
    }

    /// An impl's header without its outer attributes: `unsafe impl<T> Tr for S<T> where ..`.
    fn impl_header(&self, item: &Item) -> &'t str {
        let outer_end = self.outline.attrs(item).iter().filter(|it| !it.inner).map(|it| it.range.end).max().unwrap_or(0);
        let start = outer_end.min(item.signature.end);
        item.text(self.text, Range::new(start, item.signature.end)).trim()
    }
}

/// The inside of a delimited token tree's text: `{ a }` gives ` a `.
pub fn inside(text: &str) -> &str {
    let t = text.trim();
    if t.len() >= 2 && matches!(t.as_bytes()[0], b'(' | b'[' | b'{') { &t[1..t.len() - 1] } else { t }
}

/// The call shapes a macro's rules (`body`, inside its braces) are written for, as the pinned
/// nightly reads an arm: `[unsafe] attr(..) (..) => {..}` is an attribute rule,
/// `[unsafe] derive(..) (..) => {..}` a derive rule, any other a function-like rule; arms end
/// at a `;` outside every group. Lexed (in `edition`, into `alloc`) only when the text holds
/// `attr` or `derive` at all, so most bodies are not lexed here. A body with no arm, or one
/// that does not lex, is function-like: its definition refuses it by name when called
/// ([`crate::resolve::definition_of`]).
pub fn rules_shapes(body: &str, edition: Edition, alloc: Alloc<'_>) -> MacroShapes {
    use blaze_expand::TokenKind;
    if !body.contains("attr") && !body.contains("derive") {
        return MacroShapes::FN_LIKE;
    }
    let Ok(stream) = blaze_expand::lex(body, edition, alloc) else { return MacroShapes::FN_LIKE };
    let tokens = stream.tokens();
    let ident = |at: usize| match tokens.get(at) {
        Some(t) if matches!(t.kind, TokenKind::Ident { .. }) => Some(stream.text_of(t)),
        _ => None,
    };
    let mut shapes: Option<MacroShapes> = None;
    let mut at = 0;
    while at < tokens.len() {
        let head = if ident(at) == Some("unsafe") { at + 1 } else { at };
        let opens = matches!(tokens.get(head + 1).map(|t| t.kind), Some(TokenKind::Open(_)));
        let shape = match ident(head) {
            Some("attr") if opens => CallShape::Attr,
            Some("derive") if opens => CallShape::Derive,
            _ => CallShape::FnLike,
        };
        shapes = Some(shapes.map_or(MacroShapes::of(shape), |s| s.with(shape)));
        while at < tokens.len() {
            match tokens[at].kind {
                TokenKind::Open(_) => at = stream.close_of(at).map_or(tokens.len(), |close| close + 1),
                TokenKind::Punct { char: ';', .. } => {
                    at += 1;
                    break;
                }
                _ => at += 1,
            }
        }
    }
    shapes.unwrap_or(MacroShapes::FN_LIKE)
}

/// The first line of `text` that is not blank, trimmed, at most 80 characters.
pub fn first_line(text: &str) -> &str {
    let line = text.lines().find(|it| !it.trim().is_empty()).unwrap_or("").trim();
    line.char_indices().nth(80).map_or(line, |(at, _)| &line[..at])
}

// ---------------------------------------------------------------------------------------------
// The crate root.

/// What a crate's root has from the graph: the crates named by their root modules, and the
/// standard prelude found in the finished crate.
#[derive(Clone, Debug)]
pub struct RootPlan<'t, 'a> {
    /// Sorted by name.
    pub extern_prelude: AVec<'a, (&'t str, ItemKey)>,
    /// Sorted by name.
    pub deps: AVec<'a, Dep<&'t str>>,
    /// The injected `#[macro_use] extern crate`'s root.
    pub macro_use: Option<ItemKey>,
    /// The standard prelude module, found; `Err` says why not.
    pub prelude: Option<Result<ItemKey, &'t str>>,
    /// The injected `extern crate`'s name (`std`, or `core` under `#![no_std]`).
    pub injected: Option<&'t str>,
}

/// What the crate root takes from the lowered crate.
pub struct RootInput<'r, 't, 'a> {
    pub krate: CrateItemKey,
    pub root_file: ContentId,
    pub edition: Edition,
    /// The root file's own attributes.
    pub attrs: &'r Attrs<'t, 'a>,
    pub tree: &'r Tree<'t, 'a>,
    /// The root's `#[prelude_import]`.
    pub prelude_import: Option<&'r PreludeImport<'t, 'a>>,
    /// The root's `extern crate` items (the injected one included).
    pub extern_crates: &'r [ExternCrateDecl<&'t str, Path<'a, &'t str>>],
    /// The root's `#[macro_use] extern crate` names, in order.
    pub macro_use: &'r [&'t str],
    /// Every `#[macro_export]` `macro_rules!` of the crate's text, in module order.
    pub macro_exports: &'r [(&'t str, ItemKey, MacroShapes)],
}

/// What the crate root could not make.
#[derive(Clone, Debug)]
pub enum RootNote<'t, 'a> {
    /// `#[prelude_import]` of a path that is not a path of modules from the root.
    PreludeNotModules { at: Loc, segments: AVec<'a, &'t str> },
    /// `#[prelude_import]` of `::path`: another crate's.
    PreludeOfAnotherCrate { at: Loc, segments: AVec<'a, &'t str> },
    /// The graph names a standard prelude its crate does not have.
    StdPreludeNotFound { why: &'t str },
    /// No core or std crate in the graph.
    NoStdPrelude,
    ExternCrateNotInGraph { name: &'t str },
    MacroUseNotInGraph { name: &'t str },
    /// A `#[macro_export]` name exported twice: the first kept.
    MacroExportedTwice { name: &'t str },
}

impl fmt::Display for RootNote<'_, '_> {
    /// The kind of note, as a failure's kind is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RootNote::PreludeNotModules { .. } => "#[prelude_import] path is not a path of modules",
            RootNote::PreludeOfAnotherCrate { .. } => "#[prelude_import] of another crate (no dependencies in this run)",
            RootNote::StdPreludeNotFound { .. } => "standard prelude not found in its crate",
            RootNote::NoStdPrelude => "no standard prelude: no core or std crate in the graph",
            RootNote::ExternCrateNotInGraph { .. } => "extern crate of a crate not in the graph",
            RootNote::MacroUseNotInGraph { .. } => "#[macro_use] extern crate of a crate not in the graph",
            RootNote::MacroExportedTwice { .. } => "#[macro_export] name exported twice (first kept)",
        })
    }
}

/// The crate root of the lowered crate `input` under the graph's `plan`; what it could not
/// make is pushed to `notes`.
pub fn crate_root<'t, 'a>(
    input: &RootInput<'_, 't, '_>,
    plan: &RootPlan<'t, '_>,
    notes: &mut AVec<'_, RootNote<'t, 'a>>,
    alloc: Alloc<'a>,
) -> CrateRootBuilder<'a, &'t str> {
    let attrs = input.attrs;
    let tree = input.tree;
    let root = tree.modules[0].key;
    let prelude = match input.prelude_import {
        Some(PreludeImport { abs: false, segments, loc }) => {
            // `use prelude::rust_2024::*` at the root: a path of modules from the root.
            let mut at = 0usize;
            let mut found = true;
            for segment in segments.iter() {
                if *segment == "self" || *segment == "crate" {
                    continue;
                }
                match tree.modules.iter().position(|m| m.parent == Some(at) && m.name == Some(*segment)) {
                    Some(next) => at = next,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if found {
                Some(tree.modules[at].key)
            } else {
                notes.push(RootNote::PreludeNotModules { at: *loc, segments: AVec::from_slice_in(segments, alloc) });
                None
            }
        }
        Some(PreludeImport { abs: true, segments, loc }) => {
            notes.push(RootNote::PreludeOfAnotherCrate { at: *loc, segments: AVec::from_slice_in(segments, alloc) });
            None
        }
        None => match plan.prelude {
            Some(Ok(module)) => Some(module),
            Some(Err(why)) => {
                notes.push(RootNote::StdPreludeNotFound { why });
                None
            }
            None => {
                if !attrs.no_core {
                    notes.push(RootNote::NoStdPrelude);
                }
                None
            }
        },
    };
    let head = CrateRootHead {
        krate: input.krate,
        root_module: root,
        root_file: input.root_file,
        edition: input.edition,
        no_std: attrs.no_std,
        no_core: attrs.no_core,
        arbitrary_ints: attrs.arbitrary_ints,
        prelude,
    };
    let mut cr = CrateRootBuilder::new_in(head, alloc);
    // The extern prelude: the graph's, and the root's `extern crate` items (rustc adds an
    // `extern crate` at the root to it, `extern crate self as name` too).
    let mut externs: AVec<'a, (&'t str, ItemKey)> = AVec::from_slice_in(&plan.extern_prelude, alloc);
    for e in input.extern_crates {
        let target = match e.target {
            ExternTarget::SelfCrate => Some(root),
            ExternTarget::Dep(name) => plan.deps.iter().find(|d| d.name == name).map(|d| d.root),
        };
        let bound = match (e.target, e.rename) {
            (_, Rename::To(name)) => Some(name),
            (ExternTarget::Dep(name), Rename::Same) => Some(name),
            _ => None,
        };
        match (target, bound) {
            (Some(target), Some(name)) => externs.push((name, target)),
            (None, _) => {
                if let ExternTarget::Dep(name) = e.target {
                    notes.push(RootNote::ExternCrateNotInGraph { name });
                }
            }
            _ => {}
        }
    }
    externs.sort();
    externs.dedup_by_key(|it| it.0);
    cr.extern_prelude.extend_from_slice(&externs);
    cr.deps.extend_from_slice(&plan.deps);
    // The `#[macro_use]` prelude: the injected crate's, then the root's `#[macro_use] extern
    // crate` items in order.
    if let Some(injected) = plan.macro_use {
        cr.macro_use.push(injected);
    }
    for &name in input.macro_use {
        match plan.deps.iter().find(|d| d.name == name) {
            Some(d) => {
                if !cr.macro_use.contains(&d.root) {
                    cr.macro_use.push(d.root);
                }
            }
            None => notes.push(RootNote::MacroUseNotInGraph { name }),
        }
    }
    let mut exports: AVec<'a, (&'t str, ItemKey, MacroShapes)> = AVec::new_in(alloc);
    let mut seen: HashSet<&'t str, Alloc<'a>> = HashSet::new_in(alloc);
    for &(name, def, shapes) in input.macro_exports {
        if seen.insert(name) {
            exports.push((name, def, shapes));
        } else {
            notes.push(RootNote::MacroExportedTwice { name });
        }
    }
    exports.sort();
    cr.macro_exports.extend_from_slice(&exports);
    // The tool namespaces (`blaze` among them): one table, blaze-surface's.
    let mut tools: AVec<'a, &'t str> = AVec::from_slice_in(&blaze_surface::tool_attrs::TOOLS, alloc);
    tools.extend_from_slice(&attrs.tools);
    tools.sort_unstable();
    tools.dedup();
    cr.tools.extend_from_slice(&tools);
    let mut builtin: AVec<'a, &'t str> = AVec::from_slice_in(BUILTIN_ATTRS, alloc);
    builtin.sort_unstable();
    builtin.dedup();
    cr.builtin_attrs.extend_from_slice(&builtin);
    cr
}
