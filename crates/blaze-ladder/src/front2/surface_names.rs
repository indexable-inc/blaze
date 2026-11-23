//! D189, point 4: the surface pass over the FINISHED module names. The walk is blaze-scope's
//! sans-IO [`blaze_scope::surface::file_surface`]; this file is its thin driver: the names it
//! reads ([`SurfaceNames`], over any [`NamesSource`]: the solved names or their records) and
//! one call per file ([`surface_of`]).
//!
//! Every answer is a superset (rule 3): a path the names cannot answer (a [`PathError`], an
//! unread record) is "names nothing", which the walk reads as `MacroEffect::ANY`, so the body is
//! a surface body and gets its task; never a body taken to be inside-only by a gap.
//!
//! [`PathError`]: blaze_scope::names::PathError

use core::cell::RefCell;

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_scope::macro_effect::EffectNames;
use blaze_scope::names::Written;
use blaze_scope::surface::{BodyAt, BodyNames, FileSurface, Plant, file_surface, surface_key};
use blaze_scope::task::MacroSrc;
use blaze_surface::{Namespace, PathKind};

use super::source::{MacroAt, NamesSource};

/// The finished names as the surface pass reads them. A macro's source is kept from the
/// question that named it ([`NamesSource::macro_at`] answers both at once), so
/// [`EffectNames::source`] reads no record of its own.
pub struct SurfaceNames<'n, 's, S> {
    names: S,
    sources: RefCell<HashMap<ItemKey, MacroSrc<'n>, Alloc<'s>>>,
}

impl<'n, 's, S: NamesSource<'n>> SurfaceNames<'n, 's, S> {
    pub fn new(names: S, alloc: Alloc<'s>) -> SurfaceNames<'n, 's, S> {
        SurfaceNames { names, sources: RefCell::new(HashMap::new_in(alloc)) }
    }

    /// The definition `at` names, its source kept.
    fn keep(&self, at: MacroAt<'n>) -> ItemKey {
        if let Some(src) = at.src {
            self.sources.borrow_mut().insert(at.def.key, src);
        }
        at.def.key
    }
}

impl<'n, S: NamesSource<'n>> EffectNames<'n> for SurfaceNames<'n, '_, S> {
    fn root_of(&self, def: ItemKey) -> Option<ItemKey> {
        self.names.crate_of(def).and_then(|krate| self.names.root_module(krate))
    }

    fn lookup(&self, module: ItemKey, ns: Namespace, name: &'n str) -> Option<ItemKey> {
        // `self::name` in `module`: a `$crate::a::b!` path is read in its module's own scope,
        // never in a prelude.
        let segments = [name];
        let written = Written { kind: PathKind::SelfMod, segments: &segments[..] };
        match ns {
            Namespace::Macros => self.names.macro_at(module, written).ok().map(|at| self.keep(at)),
            Namespace::Types | Namespace::Values => self.names.walk(module, ns, written).ok().and_then(|r| r.whole(1)).map(|d| d.key),
        }
    }

    fn source(&self, def: ItemKey) -> Option<MacroSrc<'n>> {
        self.sources.borrow().get(&def).copied()
    }
}

impl<'n, S: NamesSource<'n>> BodyNames<'n> for SurfaceNames<'n, '_, S> {
    fn macro_in(&self, module: ItemKey, kind: PathKind, segments: &[&'n str]) -> Option<ItemKey> {
        self.names.macro_at(module, Written { kind, segments }).ok().map(|at| self.keep(at))
    }

    fn module_in(&self, module: ItemKey, kind: PathKind, segments: &[&'n str]) -> Option<ItemKey> {
        if segments.is_empty() {
            // `use self::*;`, `use super::*;`, `use crate::*;` in a block.
            return match kind {
                PathKind::SelfMod => self.names.written_module(module, 0).ok(),
                PathKind::Super(n) => self.names.written_module(module, n).ok(),
                PathKind::Crate => self.names.crate_of(module).and_then(|krate| self.names.root_module(krate)),
                PathKind::Plain | PathKind::Abs | PathKind::DollarCrate(_) => None,
            };
        }
        self.names.walk(module, Namespace::Types, Written { kind, segments }).ok().and_then(|r| r.whole(segments.len())).map(|d| d.key)
    }
}

/// Why a file's surface record was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceError {
    /// The names hold no root module for the crate.
    NoRoot { krate: usize },
}

impl core::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SurfaceError::NoRoot { krate } => write!(f, "the surface pass (D189): crate {krate} has no root module in the finished names"),
        }
    }
}

/// One file's surface record: its key ([`surface_key`]: the crate's root, the text, each
/// body's range and module) and the record (its surface bodies and its read list). `text` is
/// the file's text in crate `krate`; `bodies` the bodies in it the caller lowers, each with its
/// module. Scratch and the record are in `alloc`.
pub fn surface_of<'n, 'o, S: NamesSource<'n>>(names: S, krate: usize, text: &'n str, bodies: &[BodyAt], plant: Plant, alloc: Alloc<'o>) -> Result<(ContentId, FileSurface<'o>), SurfaceError> {
    let root = names.root_module(krate).ok_or(SurfaceError::NoRoot { krate })?;
    let read = SurfaceNames::new(names, alloc);
    let record = file_surface(&read, root, text, bodies, plant, alloc);
    Ok((surface_key(root, text, bodies), record))
}

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, CrateItemKey};
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::names::{self, Written};
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
    use blaze_scope::surface::{BodyAt, Plant, SurfaceWhy};
    use blaze_scope::task::Serial;
    use blaze_surface::{Namespace, PathKind, Range};

    use super::surface_of;
    use crate::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};

    /// The library: `emits_impl!` writes an impl (through its `$crate` callee), `plain!` an
    /// expression.
    const DEP: &str = "pub trait Marker {}
#[doc(hidden)]
#[macro_export]
macro_rules! writes { () => { impl $crate::Marker for u8 {} } }
#[macro_export]
macro_rules! emits_impl { () => { $crate::writes!() } }
#[macro_export]
macro_rules! plain { () => { 0 } }
";

    /// The user crate: `format` renamed to `emits_impl` (D189's case), `fine` to `plain`.
    const USER: &str = "use dep::emits_impl as format;
use dep::plain as fine;
pub fn g() { format!(); }
pub fn h() { let _ = fine!(); }
pub fn main() {}
";

    struct WithDep<'t> {
        base: &'t Texts<'t>,
        dep: usize,
        files: &'t [(String, String)],
    }

    impl<'t> SourceTexts<'t> for WithDep<'t> {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            if krate == self.dep {
                return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
            }
            self.base.read(krate, path)
        }
    }

    fn line_of(text: &str, start: &str) -> Range {
        let at = text.find(start).unwrap_or_else(|| panic!("`{start}` in the text"));
        let end = text[at..].find('\n').map_or(text.len(), |e| at + e);
        Range::new(u32::try_from(at).expect("a short text"), u32::try_from(end).expect("a short text"))
    }

    /// D189's proof (2), on the real finished names of two crates: `use dep::emits_impl as
    /// format; fn g() { format!(); }` is a surface body (`format` is `emits_impl`, whose
    /// `$crate` callee writes an impl), and its twin `fine!()` (renamed `plain!`) is not.
    /// THE PLANTED BUG `Plant::SkipEffect` (the call judged by its written name, never by its
    /// definition) misses `g`: the assertion that `g` is a surface body fails under it, so the
    /// test proves the lookup. Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.
    /// Prints `surface: g: Emits`, `surface: h: no` and `surface: reads N`.
    #[test]
    fn a_renamed_macro_makes_a_surface_body_on_real_names() {
        let alloc = blaze_own::testing::heap().alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let shelf = format!("{home}/.blaze/v2/toolchain");
        let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
        let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
        let user_files = [("/user/src/main.rs".to_string(), USER.to_string())];
        let dep_files = [("/dep/src/lib.rs".to_string(), DEP.to_string())];
        let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let mut crates: Vec<CrateInput<'_>> = inputs.crates();
        let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
        let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
        let dep = crates.len();
        let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("dep", dep)].into_boxed_slice());
        let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
        crates[0].deps = user_deps;
        crates.push(CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("blaze-ladder surface renamed test crate", b"dep")),
            edition: crate_edition,
            implicit,
            root: "/dep/src/lib.rs",
            shown_from: "/dep/",
            features: &[],
            deps: &[],
            cfg,
            script: None,
        });
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let texts = WithDep { base: &inputs.texts, dep, files: &dep_files };
        let par = crate::crates::ScratchSerial::new();
        let solved = resolve_crates(&crates, sysroot, &texts, &par, erased.bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"));
        let root = solved.root(0);
        // `g` and `h` are in the root module: their module is the root.
        let named = names::path(&solved, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &["g"] }, alloc).unwrap_or_else(|e| panic!("`g`: {e}"));
        assert!(named.whole(1).is_some(), "`g` is named in the user crate");
        let bodies = [BodyAt { range: line_of(USER, "pub fn g"), module: root }, BodyAt { range: line_of(USER, "pub fn h"), module: root }];

        let (key, real) = surface_of(&solved, 0, USER, &bodies, Plant::None, alloc).unwrap_or_else(|e| panic!("{e}"));
        let g = real.rows.iter().find(|r| r.body == 0).map(|r| r.why);
        println!("surface: g: {}", g.map_or("no".to_string(), |w| format!("{w:?}")));
        println!("surface: h: {}", if real.is_surface(1) { "yes" } else { "no" });
        println!("surface: reads {}", real.reads.len());
        assert!(matches!(g, Some(SurfaceWhy::Emits { .. })), "`format!` is `emits_impl!`: g is a surface body: {:?}", real.rows);
        assert!(!real.is_surface(1), "the twin: `fine!` is `plain!`, an expression: {:?}", real.rows);
        assert!(!real.reads.is_empty(), "the record names what it read");

        let (again_key, again) = surface_of(&solved, 0, USER, &bodies, Plant::None, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(again_key, key, "the key is the text's, made before the work");
        assert_eq!(again.reads_digest(), real.reads_digest(), "two builders of one key write one read list");

        let (_, planted) = surface_of(&solved, 0, USER, &bodies, Plant::SkipEffect, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert!(!planted.is_surface(0), "the planted bug (no MacroEffect lookup) must miss `g`, or this test proves nothing");
    }
}
