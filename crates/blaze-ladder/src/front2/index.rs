//! The program's impl and lang-item indexes, built once from the names library's per-crate
//! tables (`tables.impls`: every impl of a crate, `tables.langs`: every `#[lang]` item), never
//! by scanning texts (rule 30: the library found them; this only gathers them across crates).
//!
//! An impl's header is lowered when selection first asks for its trait or self shape
//! (`items.rs`); here an impl is only its crate, its key and its module.

use std::alloc::Global;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_scope::resolve::Names;

/// One impl of the program's crates.
#[derive(Clone, Copy, Debug)]
pub struct ImplAt<'n> {
    pub krate: usize,
    pub key: LibKey,
    pub module: LibKey,
    /// The header's text as the library read it (`impl<T> Tr for S<T>`), for messages.
    pub header: &'n str,
}

/// A lang item declared twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LangTwice {
    pub name: std::string::String,
    pub first: LibKey,
    pub second: LibKey,
    /// Each declaring crate's name and root file (rule 19: the refusal names both).
    pub crates: [(std::string::String, std::string::String); 2],
}

impl core::fmt::Display for LangTwice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [(a, a_root), (b, b_root)] = &self.crates;
        write!(f, "the lang item `{}` is declared twice: {:x} (crate `{a}`, {a_root}) and {:x} (crate `{b}`, {b_root})", self.name, self.first, self.second)
    }
}

/// The lang items by name, without the crate each was found in (kept only to name a clash).
fn keys_of<'n>(found: HashMap<&'n str, (LibKey, usize), Global>) -> HashMap<&'n str, LibKey, Global> {
    let mut langs = HashMap::new_in(Global);
    for (name, (key, _)) in found {
        langs.insert(name, key);
    }
    langs
}

/// Every impl, and every lang item by its name.
pub struct Index<'n> {
    pub impls: Vec<ImplAt<'n>, Global>,
    langs: HashMap<&'n str, LibKey, Global>,
}

impl<'n> Index<'n> {
    pub fn new(names: &'n Names<'n>) -> Result<Index<'n>, LangTwice> {
        let mut impls = Vec::new_in(Global);
        let mut langs: HashMap<&'n str, (LibKey, usize), Global> = HashMap::new_in(Global);
        let named = |krate: usize| {
            let c = &names.crates()[krate];
            (std::string::String::from(c.name), c.files.first().map_or(std::string::String::from("no file"), |file| std::string::String::from(file.path)))
        };
        // A `#![no_core]` program (the program's crate is index 0, `Front2::new`) links no
        // toolchain crate (crates.rs `Toolchain::none`): its own lang items are the program's,
        // and a toolchain crate resolved beside it declares none of them. Owed: such a program
        // resolves no toolchain crate at all (run.rs `run_program` opens the shelf first).
        let no_core = names.crates().first().is_some_and(|c| c.root_attrs.no_core);
        for (krate, c) in names.crates().iter().enumerate() {
            if no_core && krate != 0 {
                continue;
            }
            // A proc-macro unit (the unit graph's kind, carried as `implicit.proc_macro`) runs
            // at build time in the sandbox; its items are never linked into the program, so
            // none of its impls can answer a question of the program's.
            if c.implicit.proc_macro {
                continue;
            }
            impls.extend(c.tables.impls.iter().map(|i| ImplAt { krate, key: i.key, module: i.module, header: i.header }));
            for l in c.tables.langs.iter() {
                match langs.get(l.name) {
                    Some(&(first, in_crate)) if first != l.key => {
                        return Err(LangTwice { name: l.name.into(), first, second: l.key, crates: [named(in_crate), named(krate)] });
                    }
                    Some(_) => {}
                    None => {
                        langs.insert(l.name, (l.key, krate));
                    }
                }
            }
        }
        // By key: the order selection reads them in is a function of the records.
        impls.sort_by_key(|i| i.key);
        Ok(Index { impls, langs: keys_of(langs) })
    }

    /// The lang items the crates `lang_crates` declare, from any source of names, and no impl
    /// (`impls` stays empty: over records an impl is filed by `buckets::Buckets::lazy` when a
    /// question's home admits its crate, never listed up front). A proc-macro unit's lang rows
    /// are skipped as [`Index::new`] skips the unit. Crates are read in the order given, so
    /// which of two clashing declarations is `first` is a function of that order alone.
    pub fn from_source<S: super::source::NamesSource<'n>>(names: S, lang_crates: &[usize]) -> Result<Index<'n>, LangTwice> {
        let mut langs: HashMap<&'n str, (LibKey, usize), Global> = HashMap::new_in(Global);
        let mut twice: Option<LangTwice> = None;
        // A source of names by records holds no crate's name: its crate key and index name it.
        let named = |krate: usize| (std::format!("{:?}", names.crate_key(krate)), std::format!("crate {krate} of the records"));
        for &krate in lang_crates {
            if names.proc_macro(krate) == Some(true) {
                continue;
            }
            names.each_lang(krate, &mut |name, key| match langs.get(name) {
                Some(&(first, in_crate)) if first != key => {
                    if twice.is_none() {
                        twice = Some(LangTwice { name: name.into(), first, second: key, crates: [named(in_crate), named(krate)] });
                    }
                }
                Some(_) => {}
                None => {
                    langs.insert(name, (key, krate));
                }
            });
            if let Some(twice) = twice {
                return Err(twice);
            }
        }
        Ok(Index { impls: Vec::new_in(Global), langs: keys_of(langs) })
    }

    /// The item `#[lang = "name"]` declares.
    pub fn lang(&self, name: &str) -> Option<LibKey> {
        self.langs.get(name).copied()
    }

    /// The fn the back half calls on its own account as `l`, under any of the names a library
    /// declares it by ([`crate::front::FnLang::declared_as`], the pinned core's first). Two
    /// different items under two of its names is no answer: [`Index::fn_lang_clash`] names it.
    pub fn fn_lang(&self, l: crate::front::FnLang) -> Option<LibKey> {
        if self.fn_lang_clash(l).is_some() {
            return None;
        }
        l.declared_as().iter().find_map(|n| self.lang(n))
    }

    /// Two different items declaring fn lang `l` under two of its names: the two.
    pub fn fn_lang_clash(&self, l: crate::front::FnLang) -> Option<(LibKey, LibKey)> {
        let mut found = l.declared_as().iter().filter_map(|n| self.lang(n));
        let first = found.next()?;
        found.find(|k| *k != first).map(|second| (first, second))
    }

    /// The `#[lang]` name `key` is declared under, if any (a scan of the few lang items).
    pub fn lang_name_of(&self, key: LibKey) -> Option<&'n str> {
        self.langs.iter().find(|(_, k)| **k == key).map(|(n, _)| *n)
    }

    pub fn lang_count(&self) -> usize {
        self.langs.len()
    }
}
