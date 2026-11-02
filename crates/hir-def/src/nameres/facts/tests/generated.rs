//! The per-module resolver against the old collector on generated crates: module trees of
//! inline and file modules, items, plain and glob imports with cycles, shadowing (glob against
//! explicit, two globs), `pub(crate)`, `pub(super)`, `pub(in path)`, `macro_rules!` with
//! `#[macro_use]` and `#[macro_export]`, `$crate`, `extern crate self as`, cfg'd items, and
//! macros that make modules and imports, some calling each other.
//!
//! A case is a seed ([`SplitMix64`]). A failing case is shrunk on the model, not the text: modules (with
//! everything in them) and items are dropped one at a time while the difference stays, and the
//! panic shows the smallest fixture that still differs.

use std::{
    fmt::Write as _,
    panic::{self, AssertUnwindSafe},
};

use blaze_ids::rng::SplitMix64;

use super::{Exempt, compare_fixture};

pub(in crate::nameres) const MODULE_NAMES: &[&str] = &["a", "b", "c", "d", "e"];
pub(in crate::nameres) const TYPE_NAMES: &[&str] = &["S", "T", "U"];
pub(in crate::nameres) const VALUE_NAMES: &[&str] = &["f", "g"];
const EDITIONS: &[&str] = &["2015", "2018", "2021"];

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) enum Vis {
    Private,
    Pub,
    Crate,
    Super,
    /// `pub(in crate::<path>)`, an ancestor of the module.
    In(Vec<&'static str>),
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) enum UseTail {
    Glob,
    Name(&'static str),
    Alias(&'static str, &'static str),
    Underscore(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) enum Gen {
    /// A child module, by its index in [`Model::modules`].
    Mod(usize),
    Struct { name: &'static str, vis: Vis },
    Fn { name: &'static str, vis: Vis },
    Use { vis: Vis, path: String, tail: UseTail },
    /// The definition of macro `m<id>` ([`Model::macros`]).
    MacroRules { id: usize, export: bool },
    /// A call of `m<id>`, by name (textual scope) or through `crate::` (exported).
    Call { id: usize, qualified: bool },
    ExternCrateSelf { alias: &'static str },
    Cfg { on: bool, item: Box<Gen> },
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) enum MacroBody {
    /// `pub struct <name>;`
    Item(&'static str),
    /// `pub mod <name> { pub struct G; pub use $crate::*; }`
    Module(&'static str),
    /// `pub use $crate::<path>::*;`
    GlobOf(String),
    /// `pub mod <name> { $crate::m<id>!(); }`: a module whose items another exported macro makes.
    Nested(&'static str, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) struct Module {
    pub(in crate::nameres) parent: Option<usize>,
    pub(in crate::nameres) name: &'static str,
    /// Written `mod name { .. }` rather than `mod name;` and a file of its own.
    pub(in crate::nameres) inline: bool,
    /// `#[macro_use]` on its declaration: its `macro_rules!` stay in scope after it.
    pub(in crate::nameres) macro_use: bool,
    pub(in crate::nameres) items: Vec<Gen>,
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::nameres) struct Model {
    pub(in crate::nameres) edition: &'static str,
    pub(in crate::nameres) modules: Vec<Module>,
    pub(in crate::nameres) macros: Vec<MacroBody>,
}

fn pick<T: Copy>(rng: &mut SplitMix64, from: &[T]) -> T {
    *rng.pick(from).expect("a name list is never empty")
}

impl Model {
    pub(in crate::nameres) fn path(&self, mut module: usize) -> Vec<&'static str> {
        let mut out = Vec::new();
        while let Some(parent) = self.modules[module].parent {
            out.push(self.modules[module].name);
            module = parent;
        }
        out.reverse();
        out
    }

    pub(in crate::nameres) fn generate(rng: &mut SplitMix64) -> Model {
        let mut model = Model {
            edition: pick(rng, EDITIONS),
            modules: vec![Module {
                parent: None,
                name: "",
                inline: false,
                macro_use: false,
                items: Vec::new(),
            }],
            macros: Vec::new(),
        };
        // The tree first, so imports and macros can name any module.
        let count = 1 + rng.below(6) as usize;
        for _ in 0..count {
            let parent = rng.below(model.modules.len() as u64) as usize;
            if model.path(parent).len() >= 3 {
                continue;
            }
            let taken: Vec<&str> = model.modules[parent]
                .items
                .iter()
                .filter_map(|it| match it {
                    Gen::Mod(child) => Some(model.modules[*child].name),
                    _ => None,
                })
                .collect();
            // A taken name now and then: a module declared twice.
            let name = match MODULE_NAMES.iter().find(|it| !taken.contains(*it)) {
                Some(&free) if !rng.one_in(8) => free,
                _ => pick(rng, MODULE_NAMES),
            };
            let inline = rng.one_in(2);
            let macro_use = rng.one_in(3);
            model.modules.push(Module { parent: Some(parent), name, inline, macro_use, items: Vec::new() });
            let child = model.modules.len() - 1;
            model.modules[parent].items.push(Gen::Mod(child));
        }
        for _ in 0..rng.below(4) {
            let body = match rng.below(4) {
                0 => MacroBody::Item(pick(rng, TYPE_NAMES)),
                1 => MacroBody::Module(pick(rng, MODULE_NAMES)),
                2 => {
                    let target = rng.below(model.modules.len() as u64) as usize;
                    MacroBody::GlobOf(model.path(target).join("::"))
                }
                _ if !model.macros.is_empty() => MacroBody::Nested(
                    pick(rng, MODULE_NAMES),
                    rng.below(model.macros.len() as u64) as usize,
                ),
                _ => MacroBody::Item(pick(rng, TYPE_NAMES)),
            };
            model.macros.push(body);
        }
        // Every macro is defined once, exported or not, in some module.
        for id in 0..model.macros.len() {
            let module = rng.below(model.modules.len() as u64) as usize;
            let export = rng.one_in(2) || matches!(model.macros[id], MacroBody::Nested(..));
            let items = &mut model.modules[module].items;
            let at = rng.below(items.len() as u64 + 1) as usize;
            items.insert(at, Gen::MacroRules { id, export });
        }
        for module in 0..model.modules.len() {
            for _ in 0..rng.below(6) {
                let item = model.item(rng, module);
                let items = &mut model.modules[module].items;
                let at = rng.below(items.len() as u64 + 1) as usize;
                items.insert(at, item);
            }
        }
        model
    }

    fn vis(&self, rng: &mut SplitMix64, module: usize) -> Vis {
        let path = self.path(module);
        match rng.below(6) {
            0 | 1 => Vis::Pub,
            2 => Vis::Crate,
            3 if !path.is_empty() => Vis::Super,
            4 if !path.is_empty() => Vis::In(path[..rng.below(path.len() as u64) as usize].to_vec()),
            _ => Vis::Private,
        }
    }

    /// A path from `module` to some module: absolute, `self`/`super`, or (2018 and later)
    /// starting at a child's name.
    fn module_path(&self, rng: &mut SplitMix64, module: usize) -> String {
        let target = rng.below(self.modules.len() as u64) as usize;
        let target_path = self.path(target);
        let here = self.path(module);
        match rng.below(4) {
            0 if target_path.starts_with(&here) && target != module => {
                let rest = &target_path[here.len()..];
                if self.edition == "2015" { format!("self::{}", rest.join("::")) } else { rest.join("::") }
            }
            1 if !here.is_empty() => {
                let parent = &here[..here.len() - 1];
                if target_path.starts_with(parent) && target_path.len() > parent.len() {
                    format!("super::{}", target_path[parent.len()..].join("::"))
                } else {
                    "super".to_owned()
                }
            }
            _ if target_path.is_empty() => "crate".to_owned(),
            _ => format!("crate::{}", target_path.join("::")),
        }
    }

    pub(in crate::nameres) fn item(&self, rng: &mut SplitMix64, module: usize) -> Gen {
        let item = match rng.below(10) {
            0 | 1 => Gen::Struct { name: pick(rng, TYPE_NAMES), vis: self.vis(rng, module) },
            2 => Gen::Fn { name: pick(rng, VALUE_NAMES), vis: self.vis(rng, module) },
            3..=6 => {
                let path = self.module_path(rng, module);
                let name = |rng: &mut SplitMix64| match rng.below(3) {
                    0 => pick(rng, VALUE_NAMES),
                    1 => pick(rng, MODULE_NAMES),
                    _ => pick(rng, TYPE_NAMES),
                };
                let tail = match rng.below(5) {
                    0 | 1 => UseTail::Glob,
                    2 => UseTail::Name(name(rng)),
                    3 => UseTail::Alias(name(rng), pick(rng, TYPE_NAMES)),
                    _ => UseTail::Underscore(name(rng)),
                };
                Gen::Use { vis: self.vis(rng, module), path, tail }
            }
            7 if !self.macros.is_empty() => {
                Gen::Call { id: rng.below(self.macros.len() as u64) as usize, qualified: rng.one_in(2) }
            }
            8 if module == 0 && self.edition != "2015" => {
                Gen::ExternCrateSelf { alias: pick(rng, &["this", "me"]) }
            }
            _ => Gen::Struct { name: pick(rng, TYPE_NAMES), vis: Vis::Pub },
        };
        if rng.one_in(6) && !matches!(item, Gen::Mod(_)) {
            Gen::Cfg { on: rng.one_in(2), item: Box::new(item) }
        } else {
            item
        }
    }

    /// The fixture: the root file, then one file per module written `mod name;`.
    pub(in crate::nameres) fn render(&self) -> String {
        let mut files = Vec::new();
        let mut root = String::new();
        self.render_items(0, &mut root, &mut files);
        let mut out = format!("//- /lib.rs crate:main edition:{} cfg:on\n{root}", self.edition);
        for (path, text) in files {
            _ = write!(out, "//- /{path}.rs\n{text}");
        }
        out
    }

    fn render_items(&self, module: usize, out: &mut String, files: &mut Vec<(String, String)>) {
        for item in &self.modules[module].items {
            self.render_item(item, out, files);
        }
    }

    fn render_item(&self, item: &Gen, out: &mut String, files: &mut Vec<(String, String)>) {
        let vis = |vis: &Vis| match vis {
            Vis::Private => String::new(),
            Vis::Pub => "pub ".to_owned(),
            Vis::Crate => "pub(crate) ".to_owned(),
            Vis::Super => "pub(super) ".to_owned(),
            Vis::In(path) if path.is_empty() => "pub(in crate) ".to_owned(),
            Vis::In(path) => format!("pub(in crate::{}) ", path.join("::")),
        };
        match item {
            &Gen::Mod(child) => {
                let data = &self.modules[child];
                if data.macro_use {
                    out.push_str("#[macro_use]\n");
                }
                if data.inline {
                    _ = writeln!(out, "pub mod {} {{", data.name);
                    self.render_items(child, out, files);
                    out.push_str("}\n");
                } else {
                    _ = writeln!(out, "pub mod {};", data.name);
                    let mut text = String::new();
                    self.render_items(child, &mut text, files);
                    // A module declared twice names one file, which the first declaration writes.
                    let path = self.path(child).join("/");
                    if files.iter().all(|(it, _)| *it != path) {
                        files.push((path, text));
                    }
                }
            }
            Gen::Struct { name, vis: v } => {
                _ = writeln!(out, "{}struct {name};", vis(v));
            }
            Gen::Fn { name, vis: v } => {
                _ = writeln!(out, "{}fn {name}() {{}}", vis(v));
            }
            Gen::Use { vis: v, path, tail } => {
                let tail = match tail {
                    UseTail::Glob => "*".to_owned(),
                    UseTail::Name(name) => (*name).to_owned(),
                    UseTail::Alias(name, alias) => format!("{name} as {alias}"),
                    UseTail::Underscore(name) => format!("{name} as _"),
                };
                _ = writeln!(out, "{}use {path}::{tail};", vis(v));
            }
            &Gen::MacroRules { id, export } => {
                if export {
                    out.push_str("#[macro_export]\n");
                }
                let body = match &self.macros[id] {
                    MacroBody::Item(name) => format!("pub struct {name};"),
                    MacroBody::Module(name) => {
                        format!("pub mod {name} {{ pub struct G; pub use $crate::*; }}")
                    }
                    MacroBody::GlobOf(path) if path.is_empty() => "pub use $crate::*;".to_owned(),
                    MacroBody::GlobOf(path) => format!("pub use $crate::{path}::*;"),
                    MacroBody::Nested(name, inner) => {
                        format!("pub mod {name} {{ $crate::m{inner}!(); }}")
                    }
                };
                _ = writeln!(out, "macro_rules! m{id} {{ () => {{ {body} }} }}");
            }
            &Gen::Call { id, qualified } => {
                let prefix = if qualified { "crate::" } else { "" };
                _ = writeln!(out, "{prefix}m{id}!();");
            }
            Gen::ExternCrateSelf { alias } => {
                _ = writeln!(out, "extern crate self as {alias};");
            }
            Gen::Cfg { on, item } => {
                out.push_str(if *on { "#[cfg(on)]\n" } else { "#[cfg(not(on))]\n" });
                self.render_item(item, out, files);
            }
        }
    }

    /// Every model with one module (and what is in it) or one item fewer.
    fn smaller(&self) -> Vec<Model> {
        let mut out = Vec::new();
        for (module, data) in self.modules.iter().enumerate() {
            for at in 0..data.items.len() {
                let mut smaller = self.clone();
                smaller.modules[module].items.remove(at);
                out.push(smaller);
            }
        }
        for (id, _) in self.macros.iter().enumerate() {
            if self.modules.iter().all(|it| {
                it.items.iter().all(|item| !matches!(item, Gen::Call { id: called, .. } if *called == id))
            }) {
                continue;
            }
            let mut smaller = self.clone();
            for data in &mut smaller.modules {
                data.items.retain(|item| !matches!(item, Gen::Call { id: called, .. } if *called == id));
            }
            out.push(smaller);
        }
        out
    }
}

/// The differences between the old and the new def map of `fixture`, or the panic.
fn differences(fixture: &str) -> String {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut out = String::new();
        compare_fixture("generated", fixture, &Exempt::NONE, &mut out);
        out
    }));
    match result {
        Ok(out) => out,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                .unwrap_or_else(|| "(no message)".to_owned());
            format!("panicked: {message}")
        }
    }
}

/// Drops modules and items of `model` while its fixture still differs; the smallest one found.
fn shrink(mut model: Model) -> (Model, String) {
    let mut found = differences(&model.render());
    'outer: loop {
        for smaller in model.smaller() {
            let diff = differences(&smaller.render());
            if !diff.is_empty() {
                model = smaller;
                found = diff;
                continue 'outer;
            }
        }
        return (model, found);
    }
}

const CASES: u64 = 256;

#[test]
fn generated_crates_agree() {
    for case in 0..CASES {
        let seed = 0x9e37_79b9_7f4a_7c15 ^ (case + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let model = Model::generate(&mut SplitMix64::new(seed));
        let fixture = model.render();
        if differences(&fixture).is_empty() {
            continue;
        }
        let (model, diff) = shrink(model);
        panic!(
            "case {case} (seed {seed:#x}): old and new def maps differ; shrunk to\n{}\n{diff}",
            model.render()
        );
    }
}

#[test]
fn rendering_writes_every_kind_of_item() {
    let mut seen = [false; 8];
    for case in 0..CASES {
        let model = Model::generate(&mut SplitMix64::new(case));
        for data in &model.modules {
            for item in &data.items {
                let kind = match item {
                    Gen::Mod(_) => 0,
                    Gen::Struct { .. } | Gen::Fn { .. } => 1,
                    Gen::Use { tail: UseTail::Glob, .. } => 2,
                    Gen::Use { .. } => 3,
                    Gen::MacroRules { .. } => 4,
                    Gen::Call { .. } => 5,
                    Gen::ExternCrateSelf { .. } => 6,
                    Gen::Cfg { .. } => 7,
                };
                seen[kind] = true;
            }
        }
        let fixture = model.render();
        assert!(fixture.starts_with("//- /lib.rs crate:main"), "{fixture}");
    }
    assert_eq!(seen, [true; 8], "the generator never writes some kind of item");
}

#[test]
fn shrinking_keeps_a_difference_and_drops_the_rest() {
    let mut rng = SplitMix64::new(7);
    let mut model = Model::generate(&mut rng);
    while model.modules[0].items.len() < 3 {
        model = Model::generate(&mut rng);
    }
    // A stand-in for a difference: the root still declares a `pub struct S`.
    let marker = Gen::Struct { name: "S", vis: Vis::Pub };
    model.modules[0].items.push(marker.clone());
    let keeps = |model: &Model| model.modules[0].items.contains(&marker);
    let mut current = model;
    'outer: loop {
        for smaller in current.smaller() {
            if keeps(&smaller) {
                current = smaller;
                continue 'outer;
            }
        }
        break;
    }
    assert_eq!(current.modules[0].items, [marker]);
}
