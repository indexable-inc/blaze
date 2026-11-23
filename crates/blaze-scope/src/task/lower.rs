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
use blaze_own::{AString, AVec, Alloc, BString, Bump};
use blaze_surface::{
    AttrKind, BuiltinAttr, BuiltinDerive, CallRole, CallShape, ChildModDecl, CrateRootBuilder, CrateRootHead, DeclaredItem,
    DeclsBuilder, Def, DefKind, Dep, Edition, ExternCrateDecl, ExternTarget, Item, ItemDetail, ItemIndex, ItemKind,
    MacroCallDecl, MacroRulesDecl, MacroShapes, Namespace, Path, PathKind, ProcKind, Range, Rename, UseDecl, UseForm,
    WrittenVis,
};

use blaze_surface::builtin_attrs::{BUILTIN_ATTRS, PRELUDE_ATTR_MACROS, read_by_toolchain};
use blaze_surface::tool_attrs::TOOLS;

use super::Variant;
use super::attr_value::{AttrCall, AttrValueRefusal, attr_string, read_call};

mod helper_checks;

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
    /// rustc's `target_has_reliable_f128`: `f128` works as a type, its arithmetic and conversions
    /// lowered by the back end or the library's soft-float routines.
    pub reliable_f128: bool,
    /// rustc's `target_has_reliable_f128_math`: the C math functions on `f128` exist too (false
    /// on Apple aarch64, where `long double` is `double`).
    pub reliable_f128_math: bool,
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
        // The pinned rustc's `--print cfg` for the target (pins/answers/rustc.json,
        // `nightly_cfg`) holds `target_has_reliable_f128` and not `target_has_reliable_f128_math`.
        reliable_f128: true,
        reliable_f128_math: false,
        atomic_widths: &["8", "16", "32", "64", "ptr"],
        // The features of CPU `apple-m1`, the target spec's `"cpu"`, sorted: the pinned rustc's
        // `--print cfg` (rustc.json `cfg` and `nightly_cfg`), equal to `target_features::cpus`'s
        // set of `apple-m1` (tests/target_facts.rs asserts it). `fp-armv8` is no cfg feature.
        target_features: &[
            "aes", "crc", "dit", "dotprod", "dpb", "dpb2", "fcma", "fhm", "flagm", "flagm2", "fp16",
            "frintts", "jsconv", "lor", "lse", "lse2", "neon", "paca", "pacg", "pan", "pmuv3", "ras",
            "rcpc", "rcpc2", "rdm", "sb", "sha2", "sha3", "ssbs", "v8.1a", "v8.2a", "v8.3a", "v8.4a",
            "vh",
        ],
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

/// The weak lang items (rustc_hir weak_lang_items.rs, `weak_lang_items!`: `PanicImpl,
/// rust_begin_unwind; EhPersonality, rust_eh_personality; EhCatchTypeinfo,
/// rust_eh_catch_typeinfo`): one crate of a program defines each; another names it by a
/// `#[lang]` foreign fn, a use and no declaration ([`Lower::item`]).
const WEAK_LANG_ITEMS: [&str; 3] = ["panic_impl", "eh_personality", "eh_catch_typeinfo"];

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
        if target.reliable_f128 {
            atoms.push(("target_has_reliable_f128", None));
        }
        if target.reliable_f128_math {
            atoms.push(("target_has_reliable_f128_math", None));
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
            if let Some(path) = cells_answer(head(content)) {
                attrs.checks.push(path);
            }
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
                // A quoted value now; a macro call (`path = concat!(..)`) kept for the walk
                // and the lowering, which hold the edition (task/attr_value.rs).
                "path" => (attrs.path, attrs.path_call) = attr_string(content),
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
                // The Reference, `attributes.testing.test.enabled`: "Test functions are only
                // compiled when in test mode"; out of it the item is gone as a false `cfg`'s
                // is (quick-xml's `#[test] fn pi() { use pretty_assertions::assert_eq; .. }`
                // named a dev-dependency no library build holds).
                // `#[bench]` likewise (core's `bench` expands to nothing out of test mode).
                "test" | "bench" if self.eval("test") != Some(true) => attrs.cfg_ok = false,
                // `#[lang = stringify!($lang)]` (core's `panic_const!`): kept as a call and
                // read in `Lower::attrs` (task/attr_value.rs). `doc` and `deprecated` values
                // are not read here at all: nothing in the build depends on them.
                "lang" => (attrs.lang, attrs.lang_call) = attr_string(content),
                // `#[link_name = concat!(prefix!(), stringify!(f))]` (ring's `prefixed_item!`):
                // a value written as a call is kept for the names' fill, which expands it at the
                // item's place (crate::attr_values). A quoted value is read where it is used.
                "link_name" | "export_name" => {
                    let attr = if name == "link_name" { "link_name" } else { "export_name" };
                    if let (_, Some(call)) = attr_string(content) {
                        attrs.values.push((attr, call));
                    }
                }
                // `#[panic_handler]` files its fn in the lang rows under the attribute's own
                // name (the Reference, [panic.panic_handler.unique]: "There must be a single
                // panic_handler function in the dependency graph"): what a call of core's
                // `#[lang = "panic_impl"]` declaration is bound to (blaze-ladder
                // front2/panic_entry.rs asks the row by this name). No `#[lang]` is named so.
                "panic_handler" => attrs.lang = Some("panic_handler"),
                // `#[global_allocator]` is the prelude's attribute macro (core's
                // `#[rustc_builtin_macro] pub macro global_allocator`): it takes the item, and its
                // expansion (blaze-expand global_allocator.rs) writes the static back with the four
                // `__rust_*` allocator fns beside it, which alloc's `extern "Rust"` declarations
                // bind to (blaze-ladder front2/alloc_shim.rs). The static is also filed in the
                // lang rows under the attribute's own name, keyed as the expansion declares it
                // (`Lower::item`): the ladder asks the row by this name; two are the index's
                // `LangTwice`. No `#[lang]` is named so. The taker is the path as written, which
                // the call's item walk finds the attribute by.
                "global_allocator" => {
                    attrs.lang = Some("global_allocator");
                    let written = head(content).trim();
                    attrs.takers.push(written);
                    if attrs.attr_macro.is_none() {
                        attrs.attr_macro = Some(written);
                    }
                }
                "rustc_builtin_macro" => {
                    attrs.builtin = true;
                    // `#[rustc_builtin_macro(Default, attributes(default))]` (the pinned library,
                    // core/src/default.rs): the name, then the helpers as a proc derive writes
                    // them (`ProcAttr::read`'s split).
                    let mut parts = SplitTop::new(args(content));
                    let arg = parts.next().map_or("", str::trim);
                    if !arg.is_empty() {
                        attrs.builtin_name = Some(arg);
                    }
                    attrs.builtin_helpers = parts.map(str::trim).find(|part| head(part) == "attributes").map_or("", args);
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
                "feature" => {
                    for feature in SplitTop::new(args(content)) {
                        let feature = feature.trim();
                        if !feature.is_empty() {
                            attrs.features.push(feature);
                        }
                    }
                }
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
                    // The tool's segment trimmed (`tool_segment`): an expansion's text spaces
                    // its tokens (`rustfmt :: skip`, alloy-sol-macro's `BYTECODE` static), and a
                    // tool's attribute is inert however its path is spaced (task/tool_attr_spaced.rs).
                    // A tool here is one of `TOOLS`; a tool the crate root registers is known to
                    // the lowering alone (`LowerCx::tools`), which takes the first of `takers`
                    // whose first segment is none of the crate's tools (`Lower::attrs`). An item's
                    // own `register_tool` registers nothing (the Reference, `attributes.tool`:
                    // a crate-level attribute).
                    let tool = tool_segment(name).map(|first| TOOLS.contains(&first));
                    let candidate = match tool {
                        Some(tool) => !tool,
                        None => !is_builtin_attr(name) && attrs.derives.is_empty(),
                    };
                    if candidate {
                        attrs.takers.push(name);
                        if attrs.attr_macro.is_none() {
                            attrs.attr_macro = Some(name);
                        }
                    }
                    // A path no tool of `TOOLS` owns is a taker wherever it is written, so after a
                    // derive it is one of the item's after-derive calls too (derive_where's
                    // `#[::derive_where::derive_where_visited]`); `Lower::attrs` drops the paths
                    // of the crate's registered tools.
                    if tool.map_or(!is_builtin_attr(name), |tool| !tool) {
                        attrs.others.push(name);
                        attrs.derives_before.push(attrs.derives.len());
                    }
                }
            }
        }
    }
}

/// The attribute's path as written when the cells answer it, not the lowering (names-G2-G3-2):
/// a one-segment built-in attribute's name (the built-in attribute scope: `Outcome::Inert`, or
/// E0659 when a nearer scope binds an attribute macro of the name), or a path whose first
/// segment is a tool's (`#[rustfmt::skip]`, Reference `attributes.tool`: inert). Not sent:
/// `cfg`, `cfg_attr` and `unsafe(..)`, read before any name exists (rustc strips them before
/// resolution; the attribute inside `unsafe(..)` is read next and sent); the prelude's
/// attribute macros ([`PRELUDE_ATTR_MACROS`]); a `rustc_` name (no table lists them; only the
/// toolchain's crates write them); a derive's helper (sent by `Lower::helper_checks`, after the
/// item's derives, names-attr-4).
fn cells_answer(path: &str) -> Option<&str> {
    let path = path.trim();
    match tool_segment(path) {
        Some(first) => TOOLS.contains(&first).then_some(path),
        None => {
            let read_here = matches!(path, "cfg" | "cfg_attr" | "unsafe") || PRELUDE_ATTR_MACROS.contains(&path);
            (BUILTIN_ATTRS.contains(&path) && !read_here).then_some(path)
        }
    }
}

/// An attribute path's first segment, trimmed, when it has two or more: the segment a tool
/// would be (an expansion's text spaces its tokens, `rustfmt :: skip`: e0425-1). The one place
/// the lowering splits an attribute path for a tool: the taker test in `Cfg::attr` and
/// [`cells_answer`] both read it.
fn tool_segment(path: &str) -> Option<&str> {
    path.split_once("::").map(|(first, _)| first.trim())
}

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
    /// `lang = <macro call>`, read by [`read_call`] where the edition is known.
    pub lang_call: Option<AttrCall<'t>>,
    /// `path = <macro call>`, read by [`read_call`] where the edition is known.
    pub path_call: Option<AttrCall<'t>>,
    /// `link_name = <macro call>` and `export_name = <macro call>`, by the attribute's name:
    /// expanded at the item's place once the crate's names are solved (crate::attr_values).
    pub values: AVec<'a, (&'static str, AttrCall<'t>)>,
    pub builtin: bool,
    pub builtin_name: Option<&'t str>,
    /// What is inside `#[rustc_builtin_macro(Name, attributes(..))]`'s `attributes(..)`, as
    /// [`ProcAttr::helpers`]: a built-in derive's helpers (names-attr-4); empty when none.
    pub builtin_helpers: &'t str,
    pub prelude_import: bool,
    pub no_implicit_prelude: bool,
    pub no_std: bool,
    pub no_core: bool,
    /// `#![blaze::arbitrary_ints]` (docs/v2/plan/design/arbitrary-ints.md).
    pub arbitrary_ints: bool,
    pub tools: AVec<'a, &'t str>,
    /// `#![feature(a, b)]`'s names, as written (a crate root's: the crate root record's
    /// `features`, which a gate of the crate's code reads).
    pub features: AVec<'a, &'t str>,
    /// Attribute names that are neither built in nor a tool's, in written order: a one-segment
    /// name is an attribute macro (the item's taker) or, after a derive, a derive's helper,
    /// which `Lower::helper_checks` sends to the cells (names-attr-4); a path is always an
    /// attribute macro (a helper is one segment).
    pub others: AVec<'a, &'t str>,
    /// For each of `others`, how many of `derives` were written before it: a derive written
    /// after an attribute macro that takes the item is refused (attr-bang-kind-2).
    pub derives_before: AVec<'a, usize>,
    /// The attributes the cells answer ([`cells_answer`]), as written, in order: sent by
    /// `Lower::attr_checks` when the item is kept.
    pub checks: AVec<'a, &'t str>,
    /// The outermost attribute macro, as written (`bstd::main`): the item is its input, and
    /// declares nothing until the call expands. `Cfg::attr` sets the first of `takers`;
    /// `Lower::attrs` sets it again knowing the crate's tools.
    pub attr_macro: Option<&'t str>,
    /// Every attribute that takes the item unless its first segment is a tool the crate root
    /// registers, outermost first (names-attr-3).
    pub takers: AVec<'a, &'t str>,
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
            lang_call: None,
            path_call: None,
            values: AVec::new_in(alloc),
            builtin: false,
            builtin_name: None,
            builtin_helpers: "",
            prelude_import: false,
            no_implicit_prelude: false,
            no_std: false,
            no_core: false,
            arbitrary_ints: false,
            tools: AVec::new_in(alloc),
            features: AVec::new_in(alloc),
            others: AVec::new_in(alloc),
            derives_before: AVec::new_in(alloc),
            checks: AVec::new_in(alloc),
            attr_macro: None,
            takers: AVec::new_in(alloc),
            proc_macro: None,
        }
    }
}

/// An attribute the lowering never takes for an item's attribute macro by its name alone: a
/// path (a tool's or a macro's, decided by `Cfg::attr` and `Lower::attrs`) or a name the
/// toolchain reads itself (`blaze_surface::builtin_attrs`: built-in attributes, the prelude's
/// attribute macros, `rustc_` names). A derive's helper (`default`) is none of these.
pub fn is_builtin_attr(name: &str) -> bool {
    name.contains("::") || read_by_toolchain(name)
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

/// What a file's own inner attributes say under `cfg`. The attributes after a false `#![cfg]`
/// are not read (the Reference, "Conditional compilation", cfg.attr.crate-level-attrs: "When a
/// crate-level cfg has a false predicate, the crate itself still exists. Any crate attributes
/// preceding the cfg are kept, and any crate attributes following the cfg are removed as well
/// as removing all of the following crate contents."): a crate root's `#![no_std]` after it is
/// not the crate's. A module file whose `#![cfg]` is false is not walked at all.
pub fn file_attrs<'t, 'a>(cfg: &Cfg<'_, '_>, outline: &Outline<'_>, text: &'t str, alloc: Alloc<'a>) -> Attrs<'t, 'a> {
    let mut attrs = Attrs::new_in(alloc);
    let mut stack = AVec::new_in(alloc);
    for attr in outline.file_attrs() {
        if attr.kind == AttrKind::Doc {
            continue;
        }
        cfg.attr(&mut attrs, strip_attr(attr.range.of(text)), &mut stack);
        if !attrs.cfg_ok {
            break;
        }
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

/// The path of `segments` as written (`abs` for a leading `::`) in the crate `krate`, its first
/// segment written by no expansion ([`to_path_marked`] with no mark).
pub fn to_path<'t, 'a>(abs: bool, segments: &[&'t str], krate: CrateItemKey, alloc: Alloc<'a>) -> Path<'a, &'t str> {
    to_path_marked(abs, segments, None, krate, alloc)
}

/// The path of `segments` as written in the crate `krate`; `first_mark` is the mark of its first
/// segment's token in an expansion's text (token-mark-2): a `$crate` or `crate` root is read by it
/// through the one resolver, [`crate::crate_roots::root_by_mark`].
pub fn to_path_marked<'t, 'a>(abs: bool, segments: &[&'t str], first_mark: Option<blaze_expand::origin_column::TextMark>, krate: CrateItemKey, alloc: Alloc<'a>) -> Path<'a, &'t str> {
    let (kind, taken) = to_path_head(abs, segments, first_mark, krate);
    Path { kind, segments: AVec::from_slice_in(&segments[taken..], alloc) }
}

/// The kind of the path of `segments` as written in the crate `krate`, and how many of its
/// leading segments the kind takes (the rest are the path's own): the one rule, read by
/// [`to_path_marked`], which copies the rest, and by a caller that keeps its segments borrowed
/// (blaze-ladder resolve.rs: a walk's scratch copies no segment). `first_mark` as there.
pub fn to_path_head(abs: bool, segments: &[&str], first_mark: Option<blaze_expand::origin_column::TextMark>, krate: CrateItemKey) -> (PathKind, usize) {
    use crate::crate_roots::Root;
    if abs {
        return (PathKind::Abs, 0);
    }
    let first = segments.first().copied();
    let root = first.map_or(Root::NotARoot, |first| crate::crate_roots::root_by_mark(first, first_mark));
    match (first, root) {
        (Some("crate"), Root::Here) => (PathKind::Crate, 1),
        (Some("crate"), Root::Crate(k)) if k == krate => (PathKind::Crate, 1),
        (_, Root::Here) => (PathKind::DollarCrate(krate), 1),
        (_, Root::Crate(k)) => (PathKind::DollarCrate(k), 1),
        (Some("self"), _) => (PathKind::SelfMod, 1),
        (Some("super"), _) => {
            let n = segments.iter().take_while(|it| **it == "super").count();
            (PathKind::Super(u8::try_from(n).unwrap_or(u8::MAX)), n)
        }
        _ => (PathKind::Plain, 0),
    }
}

/// A simple path (`a::b`, `crate::a`, `$crate::a`, `::a`); `None` for anything else.
pub fn parse_path<'t, 'a>(text: &'t str, krate: CrateItemKey, alloc: Alloc<'a>) -> Option<Path<'a, &'t str>> {
    parse_path_marked(text, krate, &|_| None, alloc)
}

/// [`parse_path`] in an expansion's text: `mark_of` gives a segment's token's mark, read for the
/// first segment ([`to_path_marked`]).
pub fn parse_path_marked<'t, 'a>(
    text: &'t str,
    krate: CrateItemKey,
    mark_of: &dyn Fn(&'t str) -> Option<blaze_expand::origin_column::TextMark>,
    alloc: Alloc<'a>,
) -> Option<Path<'a, &'t str>> {
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
    let first_mark = segments.first().copied().and_then(mark_of);
    Some(to_path_marked(abs, &segments, first_mark, krate, alloc))
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
    /// Neither `x.rs` nor `x/mod.rs` exists: both places, as the walk looked for them (the
    /// finding names them: blaze-cli names/module_missing.rs, rustc's E0583).
    NeitherFile { at: Loc, name: &'t str, rs: AString<'a>, mod_rs: AString<'a> },
    /// A `#[path]` names a file the crate does not have.
    PathFileNotFound { at: Loc, path: AString<'a> },
    /// More than [`MOST_MODULES`] modules: the rest are not walked.
    TooManyModules { at: Loc },
    /// A `#[path = <macro call>]` that does not read to a string: the module is not walked.
    PathValueRefused { at: Loc, refusal: AttrValueRefusal<'t> },
}

impl fmt::Display for WalkNote<'_, '_> {
    /// The kind of note, as a failure's kind is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WalkNote::BothFiles { .. } => "module: both x.rs and x/mod.rs exist (x.rs taken)",
            WalkNote::NeitherFile { .. } => "module: neither x.rs nor x/mod.rs exists",
            WalkNote::PathFileNotFound { .. } => "module: #[path] file not found",
            WalkNote::TooManyModules { .. } => "module: more modules than the walk takes",
            WalkNote::PathValueRefused { .. } => "module: #[path] value is a macro call that does not read to a string",
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

/// What [`walk`] reads of one module's own text to make its children: one row per `mod` item
/// it follows, in text order (one cfg'd out, or whose `#[path = m!()]` does not read, is none).
/// A one-group solve compares a member's rows with the module tree record's
/// ([`crate::tree_record`]): equal rows, and an unchanged file set, make the same children.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WalkRow<'t> {
    pub name: &'t str,
    pub inline: bool,
    /// Its `#[no_implicit_prelude]`.
    pub no_implicit_prelude: bool,
    /// Its `#[path]`, read.
    pub path: Option<&'t str>,
}

/// The [`WalkRow`]s of module `item` of the file (`None`: the file's own items) and, for a
/// file's own items, its inner `#![cfg]`'s verdict and `#![no_implicit_prelude]`: what the walk
/// read of this text alone.
pub fn walk_rows<'t, 'a: 't>(
    cfg: &Cfg<'_, '_>,
    o: &Outline<'_>,
    text: &'t str,
    item: Option<ItemIndex>,
    edition: Edition,
    out: &mut AVec<'a, WalkRow<'t>>,
    bump: Bump<'a>,
) -> Option<(bool, bool)> {
    let alloc = bump.alloc();
    // A file whose own `#![cfg]` is false has no `mod` lines to walk ([`walk`]).
    let own = item.is_none().then(|| {
        let f = file_attrs(cfg, o, text, alloc);
        (f.cfg_ok, f.no_implicit_prelude)
    });
    if own.is_some_and(|(cfg_ok, _)| !cfg_ok) {
        return own;
    }
    for (_, child) in o.children(item) {
        let ItemDetail::Mod { inline, .. } = child.detail else { continue };
        let mut attrs = item_attrs(cfg, o, child, text, alloc);
        if !attrs.cfg_ok {
            continue;
        }
        let Some(name) = child.name.map(|r| unraw(child.text(text, r))) else { continue };
        if let Some(call) = attrs.path_call.take() {
            match read_call(call, edition, bump) {
                Ok(path) => attrs.path = Some(path),
                Err(_) => continue,
            }
        }
        out.push(WalkRow { name, inline, no_implicit_prelude: attrs.no_implicit_prelude, path: attrs.path });
    }
    own
}

/// Where the inline module `name` of module `parent` of the file stands in the file's outline
/// now: the first child the walk keeps ([`walk_rows`]: a `mod` item on under the cfg) of that
/// name that is inline. A position moves with every item added before it in the file, so the
/// module tree record keeps a module's name, and a reader of the record finds it here.
pub fn inline_child(cfg: &Cfg<'_, '_>, o: &Outline<'_>, text: &str, parent: Option<ItemIndex>, name: &str, alloc: Alloc<'_>) -> Option<ItemIndex> {
    for (at, child) in o.children(parent) {
        let ItemDetail::Mod { inline: true, .. } = child.detail else { continue };
        if child.name.map(|r| unraw(child.text(text, r))) != Some(name) {
            continue;
        }
        if item_attrs(cfg, o, child, text, alloc).cfg_ok {
            return Some(at);
        }
    }
    None
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
pub fn walk<'t, 'a: 't, F: Files<'t> + ?Sized>(
    files: &F,
    cfg: &Cfg<'_, '_>,
    krate: CrateItemKey,
    name: &str,
    root: u32,
    root_dir: &str,
    edition: Edition,
    bump: Bump<'a>,
) -> Tree<'t, 'a> {
    let alloc = bump.alloc();
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
        dir: root_dir.clone_in(alloc),
        path_dir: root_dir,
        no_implicit_prelude: root_attrs.no_implicit_prelude,
        path: copied(name, alloc),
    });
    // A false crate-level `#![cfg]` removes "all of the following crate contents" (the
    // Reference, cfg.attr.crate-level-attrs; [`file_attrs`]): the root is the one module,
    // its `mod` lines not followed (turso's testing/stress/tests/shuttle_mvcc.rs,
    // `#![cfg(shuttle)]`, was walked and named `shuttle` unresolved, c1814).
    let mut at = if root_attrs.cfg_ok { 0 } else { tree.modules.len() };
    while at < tree.modules.len() {
        let m = &tree.modules[at];
        let (file, item, key) = (m.file, m.item, m.key);
        let (dir, path_dir, mpath) = (m.dir.clone_in(alloc), m.path_dir.clone_in(alloc), m.path.clone_in(alloc));
        let o = files.outline(file);
        let text = files.text(file);
        for (idx, child) in o.children(item) {
            let ItemDetail::Mod { inline, .. } = child.detail else { continue };
            let mut attrs = item_attrs(cfg, o, child, text, alloc);
            if !attrs.cfg_ok {
                continue;
            }
            let Some(name) = child.name.map(|r| unraw(child.text(text, r))) else { continue };
            let loc = Loc { file, offset: child.range.start };
            if let Some(call) = attrs.path_call.take() {
                match read_call(call, edition, bump) {
                    Ok(path) => attrs.path = Some(path),
                    Err(refusal) => {
                        tree.notes.push(WalkNote::PathValueRefused { at: loc, refusal });
                        continue;
                    }
                }
            }
            if tree.modules.len() >= MOST_MODULES {
                tree.notes.push(WalkNote::TooManyModules { at: loc });
                break;
            }
            let ckey = key_of(krate, key, "mod", name, 0);
            let mut cpath = mpath.clone_in(alloc);
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
                    dir: d.clone_in(alloc),
                    path_dir: d,
                    no_implicit_prelude: attrs.no_implicit_prelude,
                    path: cpath,
                });
                continue;
            }
            let (target, cdir, cpdir) = if let Some(p) = attrs.path {
                let target = joined(&path_dir, &[p], alloc);
                let mut parent = target.clone_in(alloc);
                pop_dir(&mut parent);
                (target, parent.clone_in(alloc), parent)
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
                        (a, sub, dir.clone_in(alloc))
                    }
                    (true, false) => {
                        tree.missing.push(b);
                        (a, sub, dir.clone_in(alloc))
                    }
                    (false, true) => {
                        tree.missing.push(a);
                        (b, sub.clone_in(alloc), sub)
                    }
                    (false, false) => {
                        tree.notes.push(WalkNote::NeitherFile { at: loc, name, rs: a.clone_in(alloc), mod_rs: b.clone_in(alloc) });
                        tree.missing.push(a);
                        tree.missing.push(b);
                        continue;
                    }
                }
            };
            let Some(cfile) = files.find(&target) else {
                tree.missing.push(target.clone_in(alloc));
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
    /// A builtin derive (`#[rustc_builtin_macro(Default, attributes(default))] macro Default`):
    /// run by blaze-expand's builtin derives. `helpers` as [`ProcAttr::helpers`], the library's
    /// own text, kept in the records (names-attr-4).
    BuiltinDerive { helpers: &'t str },
    /// A builtin attribute (`#[rustc_builtin_macro] macro test`): not run here. A derive is
    /// [`MacroBody::BuiltinDerive`].
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
    /// The file (absolute, the reader's path space) the call's text is written in: its `Loc`'s
    /// file, or the file an `include!` read on the way ([`Fragment::written`]). Where an
    /// `include!` call reads from (core `macros/mod.rs`: "relative to the current file").
    pub written: &'t str,
    pub depth: u16,
    /// An attribute call no attribute in the text names ([`IMPLICIT_MAIN`], D104): it stands
    /// outermost, and its item is the whole input (blaze-expand `ItemAttrs::implicit_call`).
    pub implicit: bool,
    /// Where the call stands in its text: what a finding in its expansion maps back through
    /// (crate::expansion_origins). `None` for a check call, which is never expanded.
    pub place: Option<crate::expansion_origins::CallInText>,
    /// Where the input's tokens come from: a file's text, or (a call written in an
    /// expansion) a range of that expansion's sealed tokens, each keeping its context
    /// (crate::nested_input). `input` is then read only for its place and length.
    pub tokens: crate::nested_input::CallTokens<'t>,
    /// A keep call's ([`CallRole::Keep`], attr-order-1): how many of its item's after-derive
    /// names ([`Attrs::others`]) the calls before it answer; its expansion's
    /// [`Fragment::kept`]. 0 for every other call.
    pub answered: u32,
    /// A derive call's: the name of the item it is written on (unraw), the self type of the
    /// impls a derive writes. What its refusal leaves a named gap at (D193 point 5:
    /// [`crate::items_record::each_refused_impl_row`], and the names record's failed row says
    /// it). `None` for every other call.
    pub on: Option<&'t str>,
}

/// The attribute macro a blaze-edition binary's root `fn main` is the input of when none is
/// written (D104; blaze-unit-graph `Implicit::entry`): bstd's `#[bstd::main]`, which the edition
/// names `std`. Absolute, so no item of the program shadows it.
pub const IMPLICIT_MAIN: &str = "::std::main";

/// The proc-macro harness call's path (D175, [`CallRole::ProcMacroHarness`]): a word no path
/// lookup reads (the cells answer the role, never the path), shown in notes.
const HARNESS_PATH: &str = "proc_macro_harness";
/// The kind tag of the harness call's key ([`key_of`]): no item kind's.
const HARNESS_TAG: &str = "proc-macro harness";
/// The name the harness's static is declared with (rustc's `sym::_DECLS`), never bound.
pub const PROC_MACRO_DECLS: &str = "_DECLS";
/// The static's text up to its list (the attributes rustc puts on it).
const HARNESS_HEAD: &str =
    "#[rustc_proc_macro_decls]\n#[used]\n#[allow(deprecated)]\npub static _DECLS: &[::proc_macro::bridge::client::Client] = &[";
/// One entry's path up to its constructor.
const HARNESS_CLIENT: &str = "::proc_macro::bridge::client::Client::";

/// The key of crate `krate`'s `_DECLS` static (D175): the static's key in the harness call's
/// expansion, the call being the root's one entry of its tag. A function of the crate's key
/// alone (rule 71): what a code-record program names it by (blaze-ladder front2
/// `proc_macro_decls`). Only a proc-macro crate (`Implicit::proc_macro`) has the item.
pub fn proc_macro_decls_key(krate: CrateItemKey) -> ItemKey {
    let call = key_of(krate, root_key(krate), HARNESS_TAG, HARNESS_PATH, 0);
    key_of(krate, call, kind_tag(ItemKind::Static), PROC_MACRO_DECLS, 0)
}

/// The symbol of a hosted program's start (bstd's `guest::start`, run.md "The hosted world").
const HOSTED_START: &str = "bstd_hosted_start";

/// Whether the root file's text exports the program's entry itself (D104): a free fn, at the
/// file's top or in inline modules, its `cfg` true, exported as `bstd_hosted_start` or `main`
/// by `#[no_mangle]`, `#[unsafe(no_mangle)]` or `#[export_name = ".."]`. Such a file writes its
/// own start, and its `fn main` is nobody's input. A loop over the outline, no recursion.
fn exports_entry(cfg: &Cfg<'_, '_>, outline: &Outline<'_>, text: &str, alloc: Alloc<'_>) -> bool {
    outline.items().iter().any(|item| {
        if !matches!(item.detail, ItemDetail::Fn { .. }) {
            return false;
        }
        let mut up = item.parent;
        while let Some(p) = up {
            let parent = outline.item(p);
            if !matches!(parent.detail, ItemDetail::Mod { inline: true, .. }) {
                return false;
            }
            up = parent.parent;
        }
        let own_name = || item.name.map(|r| unraw(item.text(text, r)));
        let exported = outline.attrs(item).iter().filter(|a| a.kind != AttrKind::Doc).find_map(|a| {
            let content = strip_attr(item.text(text, a.range));
            match head(content) {
                "no_mangle" => own_name(),
                "unsafe" if args(content).trim() == "no_mangle" => own_name(),
                "export_name" => attr_string(content).0,
                _ => None,
            }
        });
        matches!(exported, Some(HOSTED_START | "main")) && item_attrs(cfg, outline, item, text, alloc).cfg_ok
    })
}

#[derive(Clone, Copy, Debug)]
pub struct ImplSrc<'t> {
    pub module: ItemKey,
    pub key: ItemKey,
    pub header: &'t str,
    pub loc: Loc,
    /// The marks of the text `header` is a slice of ([`Fragment::marks`], token-mark-2): the
    /// items record reads each header path's root by them (R2).
    pub marks: Option<crate::crate_roots::TextMarks<'t>>,
}

/// A `fn`, `const` or `static` (an item or a member) whose text may write an `impl` in a
/// block: its outline flag says its body holds items and its text holds the word. The driver
/// scans it ([`blaze_file_items::nested::scan`]) and makes its blocks' modules
/// ([`crate::blocks`]).
#[derive(Clone, Copy, Debug)]
pub struct BodySrc<'t> {
    pub key: ItemKey,
    /// The module its names are read in.
    pub module: ItemKey,
    /// Its whole text.
    pub text: &'t str,
    pub loc: Loc,
    /// The file its text is written in ([`CallSrc::written`]): what its block modules' calls read from.
    pub written: &'t str,
    /// How many expansions (and bodies) deep its text is.
    pub depth: u16,
    /// Where it is written: the fragment (its owner's key: the module's own text, or a call's
    /// expansion) and the last ordered entry's place before it there (`0`: none). A block's
    /// module reads the textual scope from this place on outward (Reference
    /// `macro.decl.scope.textual`: a `macro_rules!` is in scope "after the definition .. up
    /// until its surrounding scope, typically a module, is closed": a body's block is inside it).
    pub place: (ItemKey, u32),
    /// The marks of the text `text` is a slice of ([`Fragment::marks`]): `None` for a body
    /// written in a file. A block's module text is rewritten from `text`; its rows are moved
    /// with each slice it copies (crate_roots `remapped`), so a local item's `crate` written for
    /// another crate's `$crate` keeps the mark that names that crate.
    pub marks: Option<crate::crate_roots::TextMarks<'t>>,
    /// The tokens that place the text `text` is a slice of ([`Fragment::tokens`]): `None` for
    /// a body written in a file. A block's module text moves them with each slice it copies
    /// ([`crate::nested_input::ExpansionTokens::moved`]), so a call in a block of a body a
    /// macro wrote takes its input as the expansion's tokens (a `$crate` in it keeps its crate).
    pub tokens: Option<crate::nested_input::ExpansionTokens<'t>>,
}

impl<'t> BodySrc<'t> {
    /// The entry for an item of `detail` and `text`, if its body may write an impl.
    fn of(
        detail: ItemDetail,
        key: ItemKey,
        module: ItemKey,
        text: &'t str,
        loc: Loc,
        written: &'t str,
        depth: u16,
        place: (ItemKey, u32),
        marks: Option<crate::crate_roots::TextMarks<'t>>,
        tokens: Option<crate::nested_input::ExpansionTokens<'t>>,
    ) -> Option<BodySrc<'t>> {
        let holds = matches!(
            detail,
            ItemDetail::Fn { body_holds_items: true } | ItemDetail::Const { body_holds_items: true } | ItemDetail::Static { body_holds_items: true }
        );
        // The words of the items that make a block a module
        // (`blaze_file_items::nested::NestedKind::makes_module`): a body without one costs
        // eight substring searches and no scan. A trait declared in a block makes one too
        // (core's `Write::write_fmt` declares `trait SpecWriteFmt` in its body), and a `mod`
        // and an `extern crate` (whose first word is `extern`).
        // A `!` too: a statement call may declare items (crate::expanded_blocks); the scan
        // decides whether one is in statement position. The one test, shared with the names
        // digest (`blaze_file_items::nested::may_write_block`, lane CX19).
        (holds && blaze_file_items::nested::may_write_block(text)).then_some(BodySrc { key, module, text, loc, written, depth, place, marks, tokens })
    }
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
    /// A `#[lang = ..]` or `#[path = ..]` whose macro-call value does not read to a string
    /// (`include_str!`, `env!`, a macro only resolution can tell): no lang row, no path.
    AttrValueRefused(AttrValueRefusal<'t>),
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
            LowerNoteKind::AttrValueRefused(refusal) => write!(f, "{refusal}"),
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
    /// The items and members whose body may write an impl in a block ([`BodySrc`]).
    pub bodies: AVec<'a, BodySrc<'t>>,
    /// The `mod` items of a module a macro wrote (or of one inside it): modules the walk did
    /// not see, for the driver to make.
    pub mods_made: AVec<'a, ModMade<'t>>,
    /// Each trait's and impl's members on under the cfg, in declaration order, by its key.
    pub members: AVec<'a, (ItemKey, AVec<'a, Member<'t>>)>,
    /// The tuple and unit structs: each is its own constructor's key.
    pub ctors: AVec<'a, ItemKey>,
    pub notes: AVec<'a, LowerNote<'t>>,
    /// The `link_name`/`export_name` values written as macro calls, each at its item's place.
    pub values: AVec<'a, crate::attr_values::ValueSite<'t>>,
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
    /// The crate's tools ([`crate_tools`]: `TOOLS` and the root's `register_tool`s): an
    /// attribute whose path starts with one never takes its item (names-attr-3).
    pub tools: &'c [&'c str],
    /// The crate's edition: a `macro_rules!` body's arms are lexed in it ([`rules_shapes`]).
    pub edition: Edition,
    pub cfg: &'c Cfg<'c, 'c>,
    /// The modules the walk found.
    pub walked: &'c W,
    /// Where its procedural macros are declared from.
    pub procs: ProcDecls<'p>,
    /// D104: this is the root module's own fragment of a crate whose `Implicit::entry` is set:
    /// its top-level `fn main` is [`IMPLICIT_MAIN`]'s input unless an attribute macro is
    /// written on it or the file exports its own entry ([`exports_entry`]). False for every
    /// other module and every expansion.
    pub entry: bool,
    /// D175: this is the root module's own fragment of a proc-macro crate
    /// (`Implicit::proc_macro`): it ends with the crate's harness call
    /// ([`Lower::proc_macro_harness`]), whose expansion is the `_DECLS` static. False for every
    /// other module, every expansion and every other crate.
    pub harness: bool,
    /// How a derived item's derives and the names after them are sent: [`AttrOrder::Written`]
    /// for every real source (resolve.rs `SourceTexts::attr_order`).
    pub order: AttrOrder,
}

/// How a derived item's attributes are sent to the cells (attr-order-1). The Reference,
/// "Attributes" and "Procedural macros" (rustc's `take_first_attr`): an item's outer attributes
/// are taken in written order, a `cfg_attr` replaced first by what it holds; a derive runs on
/// the item as it stands when the derive is reached; the first attribute macro after it takes
/// the item with every attribute written after it, a later `#[derive]` among them unexpanded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrOrder {
    /// That order, one stage a fragment (`Lower::after_derive_takers`): the derives before the
    /// next name, the names up to the next derive, the keep call, whose fragment goes on.
    Written,
    /// The planted bug, before attr-order-1 (the control of tests/attr_order.rs): every derive
    /// of the item sent at once with all its names, so a derive written after an attribute
    /// macro would run on the item as written; the cells refuse the macro
    /// `DeriveAfterAttrMacro`, which blaze-cli printed as "no `name` in `path`".
    DerivesAtOnce,
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
    /// The file its text is written in: a file's own path; for an expansion, the file an
    /// `include!` read, else the calling call's [`CallSrc::written`]. Copied into every call
    /// and body it lowers.
    pub written: &'t str,
    /// A derived item's keep call's expansion (attr-bang-kind-2, attr-order-1): `Some(n)`, the
    /// first `n` after-derive names of its one item ([`Attrs::others`]) were answered by the
    /// fragments before it, every one inert, and the derives written before them ran there; the
    /// item's attribute walk goes on from the next name (`Lower::after_derive_takers`), none of
    /// what was sent is sent again. `None`: any other fragment.
    pub kept: Option<u32>,
    /// The marks of an expansion's text (token-mark-2, crate::crate_roots): a path's first
    /// segment's mark resolves its root. `None`: a file, or a text no column carries.
    pub marks: Option<crate::crate_roots::TextMarks<'t>>,
    /// The tokens `text` was rendered from (an expansion's; crate::nested_input): a call in it
    /// takes its input as a range of them. `None`: a file, or a text rewritten after rendering.
    pub tokens: Option<crate::nested_input::ExpansionTokens<'t>>,
}

/// Lowers `fragment`'s items under `cx`.
pub fn lower<'p: 't, 't, 'a: 't, W: Walked + ?Sized>(cx: LowerCx<'_, 'p, W>, fragment: Fragment<'_, 't>, bump: Bump<'a>) -> Lowered<'t, 'a> {
    let alloc = bump.alloc();
    let entry = cx.entry
        && fragment.items.is_none()
        && fragment.depth == 0
        && matches!(fragment.at, Where::File(_))
        && !exports_entry(cx.cfg, fragment.outline, fragment.text, alloc);
    let harness = cx.harness && fragment.items.is_none() && fragment.depth == 0 && matches!(fragment.at, Where::File(_)) && fragment.members_of.is_none();
    let mut low = Lower {
        entry,
        harness,
        harnessed: AVec::new_in(alloc),
        krate: cx.krate,
        tools: cx.tools,
        edition: cx.edition,
        cfg: cx.cfg,
        walked: cx.walked,
        module: fragment.module,
        owner: fragment.owner,
        text: fragment.text,
        outline: fragment.outline,
        at: fragment.at,
        written: fragment.written,
        depth: fragment.depth,
        made: fragment.made,
        kept: fragment.kept,
        order: cx.order,
        marks: fragment.marks,
        tokens: fragment.tokens,
        pos: 0,
        procs: cx.procs,
        alloc,
        bump,
        stack: AVec::new_in(alloc),
        seen: HashMap::new_in(alloc),
        checked: HashSet::new_in(alloc),
        checks_sent: 0,
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
            bodies: AVec::new_in(alloc),
            mods_made: AVec::new_in(alloc),
            members: AVec::new_in(alloc),
            ctors: AVec::new_in(alloc),
            notes: AVec::new_in(alloc),
            values: AVec::new_in(alloc),
        },
    };
    // The entries left to lower, each a run of siblings (`at..end`): an extern block's items
    // are lowered in its place, before its next sibling.
    let outline = fragment.outline;
    if let Some(owner) = fragment.members_of {
        low.assoc(None, owner);
        return low.out;
    }
    // A file's own items under a false `#![cfg]` are not there (the Reference,
    // cfg.attr.crate-level-attrs: "removing all of the following crate contents"). The walk
    // keeps no module file whose `#![cfg]` is false, so this is the crate root's: the crate
    // exists and is empty (a proc-macro crate's harness lists nothing).
    if fragment.items.is_none() && fragment.depth == 0 && matches!(fragment.at, Where::File(_)) && !file_attrs(low.cfg, outline, low.text, alloc).cfg_ok {
        if low.harness {
            low.proc_macro_harness();
        }
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
    if low.harness {
        low.proc_macro_harness();
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
    /// [`LowerCx::entry`], and the file exports no entry of its own.
    entry: bool,
    /// [`LowerCx::harness`], and this is the root file's own fragment.
    harness: bool,
    /// Under `harness`: each `#[proc_macro*]` fn at the root, in declaration order, its kind
    /// and its name as written (a raw `r#name` kept raw): the harness's list.
    harnessed: AVec<'a, (ProcKind, &'t str)>,
    krate: CrateItemKey,
    /// [`LowerCx::tools`].
    tools: &'c [&'c str],
    edition: Edition,
    cfg: &'c Cfg<'c, 'c>,
    walked: &'c W,
    module: ItemKey,
    owner: ItemKey,
    text: &'t str,
    outline: &'c Outline<'c>,
    at: Where,
    /// [`Fragment::written`].
    written: &'t str,
    depth: u16,
    made: bool,
    /// [`Fragment::kept`].
    kept: Option<u32>,
    /// [`LowerCx::order`].
    order: AttrOrder,
    /// [`Fragment::marks`].
    marks: Option<crate::crate_roots::TextMarks<'t>>,
    /// [`Fragment::tokens`].
    tokens: Option<crate::nested_input::ExpansionTokens<'t>>,
    pos: u32,
    procs: ProcDecls<'t>,
    alloc: Alloc<'a>,
    bump: Bump<'a>,
    /// `cfg_attr`'s attributes not yet read, for [`Cfg::attr`].
    stack: AVec<'a, &'t str>,
    /// How many entries of each owner, kind and name this fragment has keyed: [`Lower::key`].
    seen: HashMap<(ItemKey, &'t str, &'t str), u32, Alloc<'a>>,
    /// The attribute checks sent, by path and textual span: [`Lower::attr_checks`].
    checked: HashSet<(&'t str, usize), Alloc<'a>>,
    /// How many of `out.decls.calls` are attribute checks.
    checks_sent: usize,
    out: Lowered<'t, 'a>,
}

impl<'t, 'a: 't, W: Walked + ?Sized> Lower<'_, 't, 'a, W> {
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

    /// Sends a kept item's `checks` (its attributes [`cells_answer`] picks) to the cells as
    /// attribute calls with no input (names-G2-G3-2): each is answered there and never
    /// expanded (`Outcome::Inert`; `Ambiguous(BuiltinAttr)`, E0659, when a nearer scope binds an
    /// attribute macro of a built-in's name; a tool's path inert as read). One per path and
    /// textual span: a one-segment attribute reads its module's scope, the same along the whole
    /// fragment, and the textual scope, which grows only at a `macro_rules!`, a `mod` or a call;
    /// so a later check of the same path in the same span reads what the first read.
    fn attr_checks(&mut self, idx: ItemIndex, at: u32, checks: &[&'t str]) {
        for &path in checks {
            let d = &self.out.decls;
            let span = d.macro_rules.len() + d.mods.len() + d.calls.len() - self.checks_sent;
            if !self.checked.insert((path, span)) {
                continue;
            }
            let Some(parsed) = parse_path_marked(path, self.krate, &|s| self.mark_of(s), self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: path }, at, path);
                continue;
            };
            let call = self.key(self.owner, "attr check", path);
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0), role: CallRole::Call });
            // No input: a check is never `Called`, so never expanded; only its place is read
            // (an error's location, blaze-cli names.rs).
            let src = CallSrc { module: self.module, owner: None, name: path, input: "", kind: CallShape::Attr, loc: self.loc(at), written: self.written, depth: self.depth, implicit: false, place: None, tokens: crate::nested_input::CallTokens::Text, answered: 0, on: None };
            self.out.calls.push((call, src));
            self.checks_sent += 1;
        }
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
        // The item's taker: the first attribute that takes it whose first segment is none of the
        // crate's tools (a tool the root registers included: the Reference, `attributes.tool`).
        // A tool's attribute is inert and the item is kept, never the input of a call the cells
        // then skip (names-attr-3: the item vanished).
        attrs.attr_macro = attrs.takers.iter().copied().find(|p| tool_segment(p).is_none_or(|t| !self.tools.contains(&t)));
        // A registered tool's path is inert, never an after-derive call.
        let mut k = 0;
        while k < attrs.others.len() {
            if tool_segment(attrs.others[k]).is_some_and(|t| self.tools.contains(&t)) {
                attrs.others.remove(k);
                attrs.derives_before.remove(k);
            } else {
                k += 1;
            }
        }
        // A value written as a macro call (`#[lang = stringify!(x)]`), read where the crate's
        // edition and the allocator are at hand (task/attr_value.rs); one that does not read
        // is refused by name and leaves the attribute unset.
        if let Some(call) = attrs.lang_call.take() {
            match read_call(call, self.edition, self.bump) {
                Ok(name) => attrs.lang = Some(name),
                Err(refusal) => self.note(LowerNoteKind::AttrValueRefused(refusal), item.range.start, call.expr),
            }
        }
        if let Some(call) = attrs.path_call.take() {
            match read_call(call, self.edition, self.bump) {
                Ok(path) => attrs.path = Some(path),
                Err(refusal) => self.note(LowerNoteKind::AttrValueRefused(refusal), item.range.start, call.expr),
            }
        }
        attrs
    }

    /// The mark of a path's first segment in this fragment's expansion text (token-mark-2). A
    /// segment that is no slice of the text (a path blaze writes itself, `KEEP_PATH`) was
    /// written by no expansion: it has none ([`crate::crate_roots::MarkMiss`]).
    fn mark_of(&self, segment: &str) -> Option<blaze_expand::origin_column::TextMark> {
        self.marks.and_then(|marks| marks.mark_of(segment).ok().flatten())
    }

    fn vis(&self, item: &Item) -> WrittenVis<Path<'a, &'t str>> {
        let path = |r| {
            parse_path_marked(item.text(self.text, r), self.krate, &|s| self.mark_of(s), self.alloc)
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

    /// D175: the proc-macro crate's harness, as rustc's (compiler/rustc_builtin_macros
    /// proc_macro_harness.rs at the pinned nightly) writes it: ONE static listing the crate's
    /// macros in declaration order, each the pinned `proc_macro::bridge::client::Client` of its
    /// fn (library/proc_macro/src/bridge/client.rs: `Client::expand1` for a derive and a
    /// function-like macro, `Client::expand2` for an attribute macro, whose fn takes two
    /// streams). The macro's name and a derive's helpers are not in it (the pinned bridge
    /// keeps them out of `Client`); they are the crate's `ProcExport`s, in the same order.
    ///
    /// It is sent as a call with no macro ([`CallRole::ProcMacroHarness`]): the cells answer
    /// it at once and resolve.rs `keep_item` lowers its input, the static's text, as its
    /// expansion, declaring no name (rustc's `_DECLS` is def-site). So the static is an
    /// ordinary item of an expansion: a text, a key ([`proc_macro_decls_key`]), a head and a
    /// body checked and evaluated as any static's, named by key from a code-record program.
    /// Its key is a function of the crate alone (rule 71); its content, this text, is a
    /// function of the root's declarations alone, made here, before any work (rule 65).
    fn proc_macro_harness(&mut self) {
        let mut text = BString::new_in(self.bump);
        text.push_str(HARNESS_HEAD);
        for &(kind, function) in self.harnessed.iter() {
            text.push_str(HARNESS_CLIENT);
            text.push_str(match kind {
                ProcKind::Derive | ProcKind::FnLike => "expand1(crate::",
                ProcKind::Attr => "expand2(crate::",
            });
            text.push_str(function);
            text.push_str("), ");
        }
        text.push_str("];\n");
        let input: &'t str = text.into_bump_str();
        let call = self.key(self.owner, HARNESS_TAG, HARNESS_PATH);
        let Some(parsed) = parse_path(HARNESS_PATH, self.krate, self.alloc) else {
            self.note(LowerNoteKind::MacroCallPathNotRead, 0, HARNESS_PATH);
            return;
        };
        let pos = self.next_pos();
        self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: None, role: CallRole::ProcMacroHarness });
        let src = CallSrc {
            module: self.module,
            owner: None,
            name: HARNESS_PATH,
            input,
            kind: CallShape::Attr,
            loc: self.loc(0),
            written: self.written,
            depth: self.depth,
            implicit: false,
            // No written text holds it: its expansion keeps no column (resolve.rs `KeepAs`).
            place: None,
            tokens: crate::nested_input::CallTokens::Text,
            answered: 0,
            on: None,
        };
        self.out.calls.push((call, src));
    }

    /// Lowers one entry; true when its children are to be lowered next, in its place (an
    /// extern block's).
    fn item(&mut self, idx: ItemIndex, item: &Item) -> bool {
        let text = self.text;
        let attrs = self.attrs(item);
        if !attrs.cfg_ok {
            // A function-like call's place still counts: calls have no name, so a call's key is
            // its place among its owner's calls, and the body side numbers a block's calls from
            // the scan alone, which cannot read a `cfg` (front2 scope_table.rs
            // `statement_call`). Counting the removed ones keeps both numberings one; a call
            // under a false `cfg` is otherwise not lowered.
            if matches!(item.detail, ItemDetail::MacroCall { .. }) {
                let name = item.name.map(|r| unraw(item.text(text, r)));
                let _ = self.key(self.owner, kind_tag(ItemKind::MacroCall), name.unwrap_or(""));
            }
            return false;
        }
        let at = item.range.start;
        let name = item.name.map(|r| unraw(item.text(text, r)));
        let vis = self.vis(item);
        let key = self.key(self.owner, kind_tag(item.detail.kind()), name.unwrap_or(""));
        let whole = &text[item.range.start as usize..item.range.end as usize];
        // A procedural macro's function under another attribute macro (alloy-sol-macro's
        // `#[proc_macro] #[proc_macro_error] pub fn sol`) is declared from its text: the
        // Reference, "Procedural Macros": a proc-macro crate exports only its procedural
        // macros, so the attribute can change the function's body, which no name outside the
        // crate reads, and not what the crate exports. Its call is not made.
        let proc_fn = attrs.proc_macro.is_some() && matches!(item.detail, ItemDetail::Fn { .. });
        // D104: a blaze binary's root `fn main` with no attribute macro written is the input of
        // `std::main`, as if `#[::std::main]` stood outermost: the same call as a written one.
        let implicit = self.entry
            && !proc_fn
            && attrs.attr_macro.is_none()
            && item.parent.is_none()
            && name == Some("main")
            && matches!(item.detail, ItemDetail::Fn { .. });
        // rustc's `take_first_attr` takes attributes in written order, a derive among them: the
        // first taker written after a derive (a path `::derive_where::derive_where_visited` is
        // always one) runs after that derive, on the item the derive saw. And in an expansion a
        // one-segment name may be the helper of a derive on the item the expansion came from
        // (rustc's `Scope::DeriveHelpers` walks the parent expansions: derive_where's output
        // keeps `#[derive_where(..)]`, reth-codecs' `add_arbitrary_tests` keeps
        // `#[reth_codecs(..)]`). Either way which name takes the item is the cells' answer, so
        // the item goes as after-derive calls (`Lower::after_derive_takers`), never one call.
        // In an expansion a one-segment taker written before the item's derives is such a helper
        // too (reth-codecs' `add_arbitrary_tests` writes `#[serde(..)] #[derive(Arbitrary)]`
        // back when the `derive` stood after it, attr-order-1): the cells answer it first.
        let derive_target = matches!(item.detail, ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union);
        let after_derive = derive_target
            && attrs.attr_macro.is_some_and(|taker| {
                let derived_first = attrs.others.iter().position(|&o| o == taker).is_some_and(|k| attrs.derives_before[k] > 0);
                let underived = attrs.derives.is_empty() || self.order == AttrOrder::Written;
                derived_first || (self.depth > 0 && underived && !taker.contains("::"))
            });
        let written = attrs.attr_macro.filter(|_| !proc_fn && !after_derive);
        if let Some(path) = written.or(implicit.then_some(IMPLICIT_MAIN)) {
            // The item is the call's input: what it declares is the expansion's (its derives
            // and later attribute macros too, lowered from the expansion).
            let call = self.key(self.owner, "attr", path);
            let Some(parsed) = parse_path_marked(path, self.krate, &|s| self.mark_of(s), self.alloc) else {
                self.note(LowerNoteKind::AttrMacroNotLowered { attr: path }, at, name.unwrap_or("_"));
                return false;
            };
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Attr, pos, item: Some(idx.0), role: CallRole::Call });
            let place = self.call_in_text(idx, item, whole);
            let tokens = self.call_tokens(whole, crate::nested_input::InputCut::Whole);
        let src = CallSrc { module: self.module, owner: None, name: path, input: whole, kind: CallShape::Attr, loc: self.loc(at), written: self.written, depth: self.depth, implicit, place, tokens, answered: 0, on: None };
            self.out.calls.push((call, src));
            // `#[global_allocator]` writes the static back first in its expansion, whose fragment
            // is owned by the call (resolve.rs `expand_decls`'s `Fragment::owner`), so the static's
            // key there is the first of its kind and name under `call`: filed here, where the
            // attribute is still on the item.
            let allocator = attrs.lang == Some("global_allocator") && prelude_attr(path) == "global_allocator";
            if let (true, ItemDetail::Static { .. }, Some(name)) = (allocator, item.detail, name) {
                let key = key_of(self.krate, call, kind_tag(item.detail.kind()), name, 0);
                let loc = self.loc(at);
                self.out.langs.push(LangSrc { name: "global_allocator", key, loc });
            }
            return false;
        }
        // A one-segment attribute written after a derive on the item itself is that derive's
        // helper (inert) or an attribute macro that takes the item: rustc takes the derive, then
        // each later attribute with the derive's helpers innermost (`take_first_attr`). Which
        // one is the cells' answer, so the item is sent as calls and kept by its keep call when
        // none took it (`Lower::after_derive_takers`, attr-bang-kind-2). One stage a fragment
        // (attr-order-1): the derives before the next name, the names up to the next derive, the
        // keep call; the keep call's own fragment (`kept`) goes on from there, and once every
        // name is answered the item is lowered here with the derives written after the last.
        let answered = self.kept.map_or(0, |n| n as usize);
        let staged = derive_target && (after_derive || (!attrs.derives.is_empty() && !attrs.others.is_empty()));
        if staged && answered < attrs.others.len() && (self.kept.is_none() || self.order == AttrOrder::Written) {
            self.after_derive_takers(idx, item, &attrs, whole, answered);
            return false;
        }
        // The item's own text, only now: an item an attribute macro takes is replaced by the
        // macro's output (the Reference, "Procedural Macros", "Attribute macros": "The item to
        // which the attribute is applied is replaced by the zero or more items in the returned
        // TokenStream"), so the input has no text of its own; the expansion's items have theirs.
        let impl_header = matches!(item.detail, ItemDetail::Impl).then(|| self.impl_header(item));
        self.out.texts.push(ItemText { key, module: self.module, item: idx, name, impl_header, text: whole });
        // Each value written as a call, at the item's place: where its macros are looked up.
        for &(attr, call) in attrs.values.iter() {
            let tokens = self.call_tokens(call.expr, crate::nested_input::InputCut::Whole);
            let loc = self.loc(at);
            let place = (self.owner, self.pos);
            self.out.values.push(crate::attr_values::ValueSite { item: key, attr, module: self.module, place, loc, written: self.written, value: call.expr, tokens });
        }
        self.attr_checks(idx, at, &attrs.checks);
        // The derives the stages before this fragment did not send: all of them on an item no
        // name stands after; none in a keep call's fragment under the planted order.
        let run = match (self.kept, self.order) {
            (Some(_), AttrOrder::DerivesAtOnce) => attrs.derives.len(),
            _ => helper_checks::ran_before(&attrs, answered),
        };
        self.derive_calls(idx, item, at, &attrs.derives[run..], whole);
        self.helper_checks(idx, item, &attrs, run);
        // A `#[lang]` on an item of an `extern` block naming a WEAK lang item is a use of the
        // item another crate defines, not a declaration of it: rustc's lang item collector takes
        // no foreign item, and its weak-items pass reads `#[lang = "panic_impl"] fn panic_impl(..);`
        // in an extern block as "this crate needs `panic_impl`" (rustc_passes weak_lang_items.rs,
        // `visit_foreign_item`; a non-weak name there is E0264, "unknown external lang item").
        // The definition is the `#[panic_handler]` fn (std's). core declares the use twice, in
        // `panic_fmt`'s and `panic_nounwind_fmt`'s bodies: filed as rows, two keys for one name
        // stopped every check that links core (front2 index.rs `LangTwice`) once those bodies'
        // blocks were written as modules.
        let foreign = item.parent.is_some_and(|p| matches!(self.outline.item(p).detail, ItemDetail::ExternBlock { .. }));
        if let Some(lang) = attrs.lang
            && !(foreign && WEAK_LANG_ITEMS.contains(&lang))
        {
            let loc = self.loc(at);
            self.out.langs.push(LangSrc { name: lang, key, loc });
        }
        if let Some(body) = BodySrc::of(item.detail, key, self.module, whole, self.loc(at), self.written, self.depth, (self.owner, self.pos), self.marks, self.tokens) {
            self.out.bodies.push(body);
        }
        match item.detail {
            ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } => {
                if let Some(name) = name.filter(|it| *it != "_") {
                    // Declared from the text only when no module's list is held.
                    let from_text = matches!(item.detail, ItemDetail::Fn { .. }) && matches!(self.procs, ProcDecls::Text);
                    if let Some(proc) = attrs.proc_macro.filter(|_| from_text) {
                        self.proc_macro(ProcExport { kind: proc.kind, name: proc.derive.unwrap_or(name), helpers: proc.helpers });
                    }
                    // The harness's list (D175): read from the text in both `ProcDecls` modes,
                    // as rustc's collector reads the root's `#[proc_macro*]` fns in order.
                    if self.harness
                        && item.parent.is_none()
                        && matches!(item.detail, ItemDetail::Fn { .. })
                        && let Some(proc) = attrs.proc_macro
                        && let Some(written) = item.name.map(|r| item.text(text, r))
                    {
                        self.harnessed.push((proc.kind, written));
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
                for (_, v) in outline.children(Some(idx)) {
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
                self.out.impls.push(ImplSrc { module: self.module, key, header, loc, marks: self.marks });
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
                            self.out.prelude = Some(PreludeImport { abs: *abs, segments: segments.clone_in(self.alloc), loc });
                        }
                        _ => self.note(LowerNoteKind::PreludeImportNotOneGlob, at, tree_text),
                    }
                    return false;
                }
                for leaf in leaves {
                    let pos = self.next_pos();
                    let first_mark = leaf.segments.first().and_then(|s| self.mark_of(s));
                    let path = to_path_marked(leaf.abs, &leaf.segments, first_mark, self.krate, self.alloc);
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
                        (MacroShapes::DERIVE, MacroBody::BuiltinDerive { helpers: attrs.builtin_helpers })
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
            ItemDetail::MacroCall { .. } => self.fn_like_call(idx, item, key, None),
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

    /// Where a call whose input is `input` (a slice of this fragment's text) stands: its outline
    /// entry and its input's bytes from the entry's first byte (item-relative, rule 71). The
    /// calling text is set when the tables take the call (`Tables::add`). `None`: `input` is no
    /// slice of the text, which `expansion_origins::write` refuses by name.
    fn call_in_text(&self, idx: ItemIndex, item: &Item, input: &str) -> Option<crate::expansion_origins::CallInText> {
        let at = crate::expansion_origins::offset_in(self.text, input)?;
        Some(crate::expansion_origins::CallInText {
            text: None,
            item: idx,
            input_start: at.checked_sub(item.range.start)?,
            input_len: u32::try_from(input.len()).ok()?,
        })
    }

    /// Where the tokens of a call's input `input` (a slice of this fragment's text) come from:
    /// a file's text, or the range of the expansion's tokens it was rendered from. A range not
    /// found is kept as the refusal it is, for the expansion to report (rule 3).
    fn call_tokens(&self, input: &'t str, cut: crate::nested_input::InputCut) -> crate::nested_input::CallTokens<'t> {
        use crate::nested_input::CallTokens;
        match self.tokens {
            None => CallTokens::Text,
            Some(tokens) => match tokens.input(input, cut) {
                Ok(input) => CallTokens::Expansion(input),
                Err(error) => CallTokens::Refused(error),
            },
        }
    }

    /// A function-like macro call `item` keyed `call`: its path declared for the solver at this
    /// position and its source kept for the fill. `owner`: the trait or impl whose body it
    /// stands in (its expansion's items are that owner's members), `None` in a module.
    fn fn_like_call(&mut self, idx: ItemIndex, item: &Item, call: ItemKey, owner: Option<ItemKey>) {
        let text = self.text;
        let at = item.range.start;
        let ItemDetail::MacroCall { path: Some(path), input: Some(input) } = item.detail else {
            self.note(LowerNoteKind::MacroCallWithoutPathOrInput, at, "");
            return;
        };
        let path_text = item.text(text, path);
        let Some(parsed) = parse_path_marked(path_text, self.krate, &|s| self.mark_of(s), self.alloc) else {
            self.note(LowerNoteKind::MacroCallPathNotRead, at, path_text);
            return;
        };
        let pos = self.next_pos();
        self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::FnLike, pos, item: None, role: CallRole::Call });
        let tokens = self.call_tokens(inside(item.text(text, input)), crate::nested_input::InputCut::Inside);
        let src = CallSrc {
            tokens,
            module: self.module,
            owner,
            name: path_text,
            input: inside(item.text(text, input)),
            place: self.call_in_text(idx, item, inside(item.text(text, input))),
            kind: CallShape::FnLike,
            // The call's place is its path's start, not the item's (an attribute before the
            // call is no part of it): where `line!()`/`column!()` in its eager input count from.
            loc: self.loc(item.in_file(path).start),
            written: self.written,
            depth: self.depth,
            implicit: false,
            answered: 0,
            on: None,
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
            self.attr_checks(idx, item.range.start, &attrs.checks);
            if let Some(body) = BodySrc::of(item.detail, key, self.module, whole, self.loc(item.range.start), self.written, self.depth, (self.owner, self.pos), self.marks, self.tokens) {
                self.out.bodies.push(body);
            }
            if is_call {
                self.fn_like_call(idx, item, key, Some(parent_key));
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
    // The root's language features (the record sorts them).
    cr.features.extend_from_slice(&attrs.features);
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
    // The tool namespaces (`blaze` among them): the lowering's own set, the same function.
    cr.tools.extend_from_slice(&crate_tools(attrs, alloc));
    // The built-in attribute scope: built-in attributes only (sorted in their one home); the
    // prelude's attribute macros (`test`, `derive`) are macros an import may shadow.
    cr.builtin_attrs.extend_from_slice(BUILTIN_ATTRS);
    cr
}

/// The crate's tool namespaces: `TOOLS` (`blaze` among them) and the root's
/// `#![register_tool(..)]`s (`root` is the root file's attributes), sorted, each once. What every
/// module's lowering reads ([`LowerCx::tools`]) and the root record holds (the cells' tools).
pub fn crate_tools<'t, 'a>(root: &Attrs<'t, '_>, alloc: Alloc<'a>) -> AVec<'a, &'t str> {
    let mut tools: AVec<'a, &'t str> = AVec::from_slice_in(&TOOLS, alloc);
    tools.extend_from_slice(&root.tools);
    tools.sort_unstable();
    tools.dedup();
    tools
}
