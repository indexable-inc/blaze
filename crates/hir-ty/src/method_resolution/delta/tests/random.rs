//! Edit scripts over a crate of four files (design §12): impls added, removed, given another
//! trait or another self type, an alias re-pointed along a chain, trait items added and
//! removed, a `use` re-pointing the trait path some impls name, derives toggled, impls added
//! to and removed from the bodies of every module, and a `macro_rules!` a body calls switched
//! between writing an impl and writing nothing, its call moved between modules. After every
//! edit each reader and each slice is checked against the reference ([`super::check`]), and
//! the maintainer's write delta against the keys and item names before and after; at the end of
//! each script every impl-set and candidate-set hash is compared with a fresh database's of the
//! same text.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use hir_def::{ImplId, TraitId};
use hir_expand::name::Name;
use intern::Symbol;
use smallvec::SmallVec;

use super::{Loaded, check, impl_keys, trait_named, trait_names};
use crate::method_resolution::delta::{LastWrite, candidate_set_hash, impl_set_hash, key::ImplKey};

const TRAITS: [&str; 2] = ["T0", "T1"];
const ITEMS: [&str; 4] = ["fn m(&self);", "fn n(&self) {}", "const K: u8;", "type Ty;"];
const ITEM_NAMES: [&str; 4] = ["m", "n", "K", "Ty"];
const STRUCTS: usize = 4;

/// The trait an impl names: one of the root's, `Tr` (module `b`'s `use` of one of them), or
/// none for an inherent impl.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TraitRef {
    Root(usize),
    Use,
    Inherent,
}

/// What an impl is for: a struct of `a`, an alias of `c` (`A0` names a struct, `A1` names `A0`),
/// or every type (a blanket impl, trait impls only).
#[derive(Clone, Copy, Debug, PartialEq)]
enum SelfTy {
    Struct(usize),
    Alias(usize),
    Blanket,
}

#[derive(Clone, Copy, Debug)]
struct Impl {
    module: usize,
    trait_: TraitRef,
    self_ty: SelfTy,
}

/// An impl in a body of a module's `holder` (`0` the root, then `a`, `b`, `c`): of a root
/// trait, or inherent, for a struct of `a`.
#[derive(Clone, Copy, Debug)]
struct BodyImpl {
    module: usize,
    trait_: Option<usize>,
    self_ty: usize,
}

#[derive(Clone, Debug)]
struct Model {
    /// Each trait's items, by index into [`ITEMS`].
    items: [Vec<usize>; 2],
    /// Which root trait `b`'s `Tr` is.
    used: usize,
    /// Which struct `c`'s `A0` is.
    alias: usize,
    derived: [bool; STRUCTS],
    bodies: Vec<BodyImpl>,
    /// Whether the root's `mk!` writes `impl crate::T0 for crate::a::S3 {}` or nothing.
    macro_writes: bool,
    /// The module whose `holder` calls `mk!()`, if any.
    macro_call: Option<usize>,
    impls: Vec<Impl>,
}

const MODULES: [&str; 3] = ["a", "b", "c"];

impl Model {
    fn start() -> Model {
        Model {
            items: [vec![0], vec![1, 3]],
            used: 0,
            alias: 0,
            derived: [true, false, false, false],
            bodies: vec![
                BodyImpl { module: 0, trait_: Some(1), self_ty: 2 },
                BodyImpl { module: 1, trait_: None, self_ty: 0 },
            ],
            macro_writes: true,
            macro_call: Some(2),
            impls: vec![
                Impl { module: 0, trait_: TraitRef::Root(0), self_ty: SelfTy::Struct(0) },
                Impl { module: 1, trait_: TraitRef::Use, self_ty: SelfTy::Struct(1) },
                Impl { module: 2, trait_: TraitRef::Inherent, self_ty: SelfTy::Alias(1) },
                Impl { module: 2, trait_: TraitRef::Root(1), self_ty: SelfTy::Blanket },
            ],
        }
    }

    /// The fixture's files: the root, then `a`, `b` and `c`.
    fn render(&self) -> [String; 4] {
        let mut root = String::from(match self.macro_writes {
            true => "macro_rules! mk { () => { impl crate::T0 for crate::a::S3 {} }; }\n",
            false => "macro_rules! mk { () => {}; }\n",
        });
        root.push_str("mod a;\nmod b;\nmod c;\n");
        for (t, name) in TRAITS.iter().enumerate() {
            let items: Vec<&str> = self.items[t].iter().map(|&i| ITEMS[i]).collect();
            _ = writeln!(root, "pub trait {name} {{ {} }}", items.join(" "));
        }
        let mut a = String::new();
        for s in 0..STRUCTS {
            if self.derived[s] {
                a.push_str("#[derive(Clone)]\n");
            }
            _ = writeln!(a, "pub struct S{s};");
        }
        let mut b = format!("use crate::{} as Tr;\n", TRAITS[self.used]);
        let mut c = format!("pub type A0 = crate::a::S{};\npub type A1 = A0;\n", self.alias);
        for imp in &self.impls {
            let self_ty = match imp.self_ty {
                SelfTy::Struct(s) => format!("crate::a::S{s}"),
                SelfTy::Alias(n) => format!("crate::c::A{n}"),
                SelfTy::Blanket => "X".to_owned(),
            };
            let generics = if imp.self_ty == SelfTy::Blanket { "<X>" } else { "" };
            let line = match imp.trait_ {
                TraitRef::Root(t) => {
                    format!("impl{generics} crate::{} for {self_ty} {{}}\n", TRAITS[t])
                }
                TraitRef::Use => format!("impl{generics} Tr for {self_ty} {{}}\n"),
                TraitRef::Inherent => format!("impl {self_ty} {{}}\n"),
            };
            [&mut a, &mut b, &mut c][imp.module].push_str(&line);
        }
        let mut files = [root, a, b, c];
        for (module, file) in files.iter_mut().enumerate() {
            let mut holder = String::new();
            for imp in self.bodies.iter().filter(|it| it.module == module) {
                let s = imp.self_ty;
                _ = match imp.trait_ {
                    Some(t) => {
                        writeln!(holder, "    impl crate::{} for crate::a::S{s} {{}}", TRAITS[t])
                    }
                    None => writeln!(holder, "    impl crate::a::S{s} {{ fn f(&self) {{}} }}"),
                };
            }
            if self.macro_call == Some(module) {
                holder.push_str("    mk!();\n");
            }
            if module == 0 || !holder.is_empty() {
                _ = write!(file, "pub fn holder() {{\n{holder}}}\n");
            }
        }
        files
    }

    fn fixture(&self) -> String {
        let [root, a, b, c] = self.render();
        format!(
            "//- minicore: sized, clone, derive\n//- /main.rs crate:main\n{root}//- /a.rs\n{a}\
             //- /b.rs\n{b}//- /c.rs\n{c}"
        )
    }

    /// One random edit; what it was.
    fn edit(&mut self, rng: &mut Rng) -> String {
        match rng.below(12) {
            0 => {
                let imp = self.random_impl(rng);
                self.impls.push(imp);
                format!("add {imp:?}")
            }
            1 if !self.impls.is_empty() => {
                let at = rng.below(self.impls.len());
                format!("remove {:?}", self.impls.remove(at))
            }
            2 if !self.impls.is_empty() => {
                let at = rng.below(self.impls.len());
                let trait_ = self.random_trait(rng, self.impls[at].module, self.impls[at].self_ty);
                self.impls[at].trait_ = trait_;
                format!("retarget {at} to {trait_:?}")
            }
            3 if !self.impls.is_empty() => {
                let at = rng.below(self.impls.len());
                let self_ty = self.random_self_ty(rng, self.impls[at].trait_);
                self.impls[at].self_ty = self_ty;
                format!("re-head {at} to {self_ty:?}")
            }
            4 => {
                self.alias = rng.below(STRUCTS);
                format!("A0 = S{}", self.alias)
            }
            5 => {
                let t = rng.below(TRAITS.len());
                let item = rng.below(ITEMS.len());
                match self.items[t].iter().position(|&it| it == item) {
                    Some(at) => _ = self.items[t].remove(at),
                    None => self.items[t].push(item),
                }
                format!("toggle `{}` of {}", ITEM_NAMES[item], TRAITS[t])
            }
            6 => {
                self.used = 1 - self.used;
                format!("Tr = {}", TRAITS[self.used])
            }
            7 => {
                let s = rng.below(STRUCTS);
                self.derived[s] = !self.derived[s];
                format!("toggle the derive of S{s}")
            }
            8 => {
                let imp = BodyImpl {
                    module: rng.below(MODULES.len() + 1),
                    trait_: [None, Some(0), Some(1)][rng.below(3)],
                    self_ty: rng.below(STRUCTS),
                };
                self.bodies.push(imp);
                format!("add {imp:?} to a body")
            }
            9 if !self.bodies.is_empty() => {
                let at = rng.below(self.bodies.len());
                format!("remove {:?} from a body", self.bodies.remove(at))
            }
            10 => {
                self.macro_writes = !self.macro_writes;
                format!("`mk!` writes an impl: {}", self.macro_writes)
            }
            _ => {
                let to = rng.below(MODULES.len() + 2);
                self.macro_call = (to <= MODULES.len()).then_some(to);
                format!("`mk!()` called in {:?}", self.macro_call)
            }
        }
    }

    fn random_impl(&self, rng: &mut Rng) -> Impl {
        let module = rng.below(MODULES.len());
        let self_ty = match rng.below(3) {
            0 => SelfTy::Struct(rng.below(STRUCTS)),
            1 => SelfTy::Alias(rng.below(2)),
            _ => SelfTy::Blanket,
        };
        let trait_ = self.random_trait(rng, module, self_ty);
        Impl { module, trait_, self_ty }
    }

    fn random_trait(&self, rng: &mut Rng, module: usize, self_ty: SelfTy) -> TraitRef {
        loop {
            let trait_ = match rng.below(3) {
                0 => TraitRef::Root(rng.below(TRAITS.len())),
                1 if module == 1 => TraitRef::Use,
                1 => continue,
                _ => TraitRef::Inherent,
            };
            if trait_ == TraitRef::Inherent && self_ty == SelfTy::Blanket {
                continue;
            }
            return trait_;
        }
    }

    fn random_self_ty(&self, rng: &mut Rng, trait_: TraitRef) -> SelfTy {
        match rng.below(3) {
            0 => SelfTy::Struct(rng.below(STRUCTS)),
            1 => SelfTy::Alias(rng.below(2)),
            _ if trait_ == TraitRef::Inherent => SelfTy::Struct(rng.below(STRUCTS)),
            _ => SelfTy::Blanket,
        }
    }
}

/// Whether the maintainer's [`LastWrite`] for `krate` names every impl whose key changed, came
/// or went since `keys`, and every trait whose item names changed since `names`. Each write is
/// checked against the state before it, so `keys` and `names` are moved on to now.
fn check_delta(
    db: &mut Db,
    krate: Crate,
    keys: &mut HashMap<ImplId, ImplKey>,
    names: &mut HashMap<TraitId, SmallVec<[Name; 8]>>,
    what: &str,
    failures: &mut String,
) {
    let delta = db.with_state::<LastWrite, _>(|_, last| {
        let at = last.crates.iter().position(|it| it.krate == krate);
        at.map(|at| (last.crates[at].keys_changed.clone(), last.crates[at].traits_changed.clone()))
    });
    let (keys_changed, traits_changed) = delta.unwrap_or_default();
    let (keys_now, names_now) = (impl_keys(db, krate), trait_names(db, krate));
    let moved = keys
        .iter()
        .filter(|&(imp, key)| keys_now.get(imp) != Some(key))
        .map(|(imp, _)| *imp)
        .chain(keys_now.keys().filter(|imp| !keys.contains_key(*imp)).copied());
    for imp in moved {
        if keys_changed.binary_search(&imp).is_err() {
            _ = writeln!(failures, "{what}: {imp:?}'s key changed but `LastWrite` misses it");
        }
    }
    let renamed = names
        .iter()
        .filter(|&(trait_, old)| names_now.get(trait_) != Some(old))
        .map(|(trait_, _)| *trait_)
        .chain(names_now.keys().filter(|it| !names.contains_key(*it)).copied());
    for trait_ in renamed {
        if traits_changed.binary_search(&trait_).is_err() {
            _ = writeln!(failures, "{what}: {trait_:?}'s names changed but `LastWrite` misses it");
        }
    }
    *keys = keys_now;
    *names = names_now;
}

/// xorshift64*: the scripts are the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % n
    }
}

/// Every hash `db` gives `krate`'s traits and item names, by the trait's name.
fn hashes(db: &Db, krate: Crate) -> Vec<String> {
    let mut out = Vec::new();
    for name in TRAITS.iter().chain(["Clone"].iter()) {
        let trait_ = trait_named_anywhere(db, krate, name);
        out.push(format!("{name}: {:?}", impl_set_hash(db, krate, trait_)));
    }
    for name in ITEM_NAMES.iter().chain(["clone", "missing"].iter()) {
        let name = Name::new_symbol_root(Symbol::intern(name));
        out.push(format!("`{}`: {:?}", name.as_str(), candidate_set_hash(db, krate, name)));
    }
    out
}

/// What `krate`'s def maps declare, as the hashes read it: each trait with its items, and each
/// module's impls by the trait they name, sorted so two databases compare.
fn declarations(db: &Db, krate: Crate) -> Vec<String> {
    use hir_def::{ModuleDefId, nameres::crate_def_map, signatures::TraitSignature};

    use crate::db::HirDatabase as _;

    let mut out = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            if let ModuleDefId::TraitId(it) = def {
                let items: Vec<&str> =
                    it.trait_items(db).items.iter().map(|(name, _)| name.as_str()).collect();
                out.push(format!("trait {}: {items:?}", TraitSignature::of(db, it).name.as_str()));
            }
        }
        for imp in scope.impls() {
            let trait_ = db.impl_trait(imp).map(|it| it.skip_binder().def_id.0);
            let name = trait_.map(|it| TraitSignature::of(db, it).name.as_str().to_owned());
            out.push(format!("impl of {}", name.as_deref().unwrap_or("(inherent)")));
        }
    }
    out.sort();
    out
}

/// Each root trait's slice as the impl-set hash reads it: every row's module, impl identity and
/// head, its derives, the body modules it lists, and the hash of their impls.
fn slice_rows(db: &Db, krate: Crate) -> Vec<String> {
    // The rows' simplified types name solver ids, whose `Debug` reads the attached database.
    crate::attach_db(db, || slice_rows_attached(db, krate))
}

fn slice_rows_attached(db: &Db, krate: Crate) -> Vec<String> {
    use crate::method_resolution::delta::{
        impl_identity,
        publish::trait_slice,
        read::{body_impl_set_hash, trait_bodies},
    };

    let mut out = Vec::new();
    for name in TRAITS {
        let trait_ = trait_named(db, krate, name);
        let Some(slice) = trait_slice(db, krate, trait_) else {
            out.push(format!("{name}: no slice"));
            continue;
        };
        let rows = slice.rows(db);
        for &(module, imp, head) in rows.impls.iter() {
            out.push(format!("{name}: {module:?} {:?} {head:?}", impl_identity(db, imp)));
        }
        out.push(format!(
            "{name}: {} derives, bodies {:?}, bodies hash {:?}",
            rows.derives.len(),
            rows.bodies,
            body_impl_set_hash(db, krate, trait_),
        ));
        for &module in rows.bodies.iter() {
            let table = trait_bodies(db, module);
            let Some(t) = table.trait_index(trait_) else {
                out.push(format!("{name}: body {module:?} has no impl of it"));
                continue;
            };
            for &imp in table.blanket.run(t) {
                out.push(format!("{name}: body {module:?} blanket {:?}", impl_identity(db, imp)));
            }
            for (self_ty, (impls, derives)) in table.rows_at(t) {
                let ids: Vec<_> = impls.iter().map(|&imp| impl_identity(db, imp)).collect();
                out.push(format!(
                    "{name}: body {module:?} {self_ty:?} {ids:?}, {} derives",
                    derives.len()
                ));
            }
        }
    }
    out
}

/// A trait of `krate` or of its dependencies by name (`Clone` is minicore's).
fn trait_named_anywhere(db: &Db, krate: Crate, name: &str) -> TraitId {
    if TRAITS.contains(&name) {
        return trait_named(db, krate, name);
    }
    let core = krate.data(db).dependencies.iter().map(|it| it.crate_id).find(|&dep| {
        super::declared_traits(db, dep).iter().any(|&it| {
            hir_def::signatures::TraitSignature::of(db, it).name.as_str() == name
        })
    });
    trait_named(db, core.unwrap_or_else(|| panic!("no dependency declares `{name}`")), name)
}

#[test]
fn every_slice_and_reader_is_the_fresh_one_after_each_edit() {
    let mut failures = String::new();
    for seed in 1..=8u64 {
        let mut model = Model::start();
        let mut loaded = Loaded::new(&model.fixture());
        let krate = loaded.krate();
        check(&loaded.db, krate, &format!("seed {seed}, start"), &mut failures);
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        for step in 0..24 {
            let before = model.render();
            let what = model.edit(&mut rng);
            let what = format!("seed {seed}, step {step}: {what}");
            let after = model.render();
            if before == after {
                continue;
            }
            let mut keys = impl_keys(&loaded.db, krate);
            let mut names = trait_names(&loaded.db, krate);
            for (file, (old, new)) in before.iter().zip(after.iter()).enumerate() {
                if old != new {
                    loaded.db.set_file_text(loaded.files[file], new);
                    let (db, failures) = (&mut loaded.db, &mut failures);
                    check_delta(db, krate, &mut keys, &mut names, &what, failures);
                }
            }
            check(&loaded.db, krate, &what, &mut failures);
        }
        let fresh = Loaded::new(&model.fixture());
        let (maintained, made) = (hashes(&loaded.db, krate), hashes(&fresh.db, fresh.krate()));
        if maintained != made {
            let kept = declarations(&loaded.db, krate);
            let fresh_krate = fresh.krate();
            let made_decls = declarations(&fresh.db, fresh_krate);
            let kept_rows = slice_rows(&loaded.db, krate);
            let made_rows = slice_rows(&fresh.db, fresh_krate);
            _ = writeln!(
                failures,
                "seed {seed}: the hashes differ from a fresh database's:\n{maintained:#?}\n\
                 {made:#?}\nthe declarations, maintained then fresh:\n{kept:#?}\n{made_decls:#?}\n\
                 the slices, maintained then fresh:\n{kept_rows:#?}\n{made_rows:#?}"
            );
        }
    }
    assert!(failures.is_empty(), "{failures}");
}
