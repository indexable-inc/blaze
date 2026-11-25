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

use super::linked::Linked;

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

/// The lang items by name, and the deterministic reverse map by key (for O(1) `lang_name_of`).
fn keys_of<'n, A: core::alloc::Allocator + Copy>(
    found: HashMap<&'n str, (LibKey, usize), A>,
) -> (HashMap<&'n str, LibKey, A>, HashMap<LibKey, &'n str, A>) {
    let alloc = *found.allocator();
    let mut langs = HashMap::with_capacity_in(found.len(), alloc);
    let mut lang_by_key: HashMap<LibKey, &'n str, A> = HashMap::with_capacity_in(found.len(), alloc);
    for (name, (key, _)) in found {
        langs.insert(name, key);
        // Pick the lexicographically smallest name if two lang names ever alias one LibKey so
        // the reverse map is independent of HashMap iteration order.
        lang_by_key
            .entry(key)
            .and_modify(|prev| {
                if name < *prev {
                    *prev = name;
                }
            })
            .or_insert(name);
    }
    (langs, lang_by_key)
}

/// Collects every `#[lang]` item declared by `lang_crates` (skipping proc-macro units) into `alloc`.
fn collect_source_langs<'n, S: super::source::NamesSource<'n>, A: core::alloc::Allocator + Copy>(
    names: S,
    lang_crates: &[usize],
    alloc: A,
) -> Result<HashMap<&'n str, (LibKey, usize), A>, LangTwice> {
    let mut langs: HashMap<&'n str, (LibKey, usize), A> = HashMap::new_in(alloc);
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
    Ok(langs)
}

/// Thread-safe, group-scoped lang-item index allocated in [`blaze_own::SharedAlloc<'static>`]
/// so a member group can build it once across all stretches with zero `Global` allocations and
/// lend it to every body and lazy head freezer via [`IndexIn::borrow`].
pub struct IndexIn {
    langs: HashMap<blaze_own::String<blaze_own::SharedAlloc<'static>>, LibKey, blaze_own::SharedAlloc<'static>>,
    lang_by_key: HashMap<LibKey, blaze_own::String<blaze_own::SharedAlloc<'static>>, blaze_own::SharedAlloc<'static>>,
    scope: blaze_own::Vec<usize, blaze_own::SharedAlloc<'static>>,
}

impl IndexIn {
    pub fn from_source_in<'n, S: super::source::NamesSource<'n>>(
        names: S,
        lang_crates: &[usize],
        alloc: blaze_own::SharedAlloc<'static>,
    ) -> Result<IndexIn, LangTwice> {
        let found = collect_source_langs(names, lang_crates, alloc)?;
        let mut scope = blaze_own::Vec::with_capacity_in(lang_crates.len(), alloc);
        scope.extend_from_slice(lang_crates);
        scope.sort_unstable();
        scope.dedup();
        let mut langs = HashMap::with_capacity_in(found.len(), alloc);
        let mut lang_by_key: HashMap<LibKey, blaze_own::String<blaze_own::SharedAlloc<'static>>, blaze_own::SharedAlloc<'static>> =
            HashMap::with_capacity_in(found.len(), alloc);
        for (name, (key, _)) in found {
            langs.insert(blaze_own::String::from_str_in(name, alloc), key);
            // Pick the lexicographically smallest name if two lang names ever alias one LibKey so
            // the reverse map is independent of HashMap iteration order.
            match lang_by_key.get_mut(&key) {
                Some(prev) if name < prev.as_str() => {
                    prev.clear();
                    prev.push_str(name);
                }
                Some(_) => {}
                None => {
                    lang_by_key.insert(key, blaze_own::String::from_str_in(name, alloc));
                }
            }
        }
        Ok(IndexIn { langs, lang_by_key, scope })
    }

    /// Borrows this shared index as an [`Index<'n>`] with zero heap allocation.
    pub fn borrow<'n>(&'n self) -> Index<'n> {
        Index { impls: Default::default(), data: IndexData::Borrowed(self) }
    }
}

struct IndexOwned<'n> {
    langs: HashMap<&'n str, LibKey, Global>,
    lang_by_key: HashMap<LibKey, &'n str, Global>,
    /// What the lang rows are a function of (with the names; rule 21): the roots of the
    /// program they were filed for ([`Linked::roots`]), or, from records, the lang crates
    /// given, sorted. Two indexes of different scope are two programs' and never shared.
    scope: Vec<usize, Global>,
}

enum IndexData<'n> {
    Owned(IndexOwned<'n>),
    Borrowed(&'n IndexIn),
}

/// Every impl, and every lang item of ONE program by its name.
pub struct Index<'n> {
    pub impls: Vec<ImplAt<'n>, Global>,
    data: IndexData<'n>,
}

impl<'n> Index<'n> {
    /// The impls and the lang items of the program `linked`: each linked crate's own rows
    /// (the names library's `tables.impls` and `tables.langs`, filed once per crate) joined
    /// over the closure. A crate the program does not link adds no row: another binary of the
    /// workspace, a test or example unit (`#[global_allocator]` and `#[panic_handler]` are one
    /// per program, not per workspace), and the macro side (a member proc macro and the
    /// wasm32 core, alloc and std only it reaches: a second core's impls of one trait for one
    /// type beside the target core's would be duplicate impls, as its lang rows were
    /// duplicate lang items). Two linked crates declaring one lang name differently is
    /// [`LangTwice`], naming both crates.
    pub fn new(names: &'n Names<'n>, linked: &Linked<'_>) -> Result<Index<'n>, LangTwice> {
        let mut impls = Vec::new_in(Global);
        let mut langs: HashMap<&'n str, (LibKey, usize), Global> = HashMap::new_in(Global);
        let named = |krate: usize| {
            let c = &names.crates()[krate];
            (std::string::String::from(c.name), c.files.first().map_or(std::string::String::from("no file"), |file| std::string::String::from(file.path)))
        };
        // One rule for impls and lang rows: the linked crates. It covers what two special
        // cases did before: a `#![no_core]` root's closure holds no toolchain crate, and a
        // proc-macro unit is never linked (it runs at build time in the sandbox), nor is what
        // only it reaches (linked.rs). A ROOT that is a proc-macro unit (a proc macro's own
        // program) is linked, as its own.
        for (krate, c) in names.crates().iter().enumerate() {
            if !linked.links(krate) {
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
        let mut scope = Vec::with_capacity_in(linked.roots().len(), Global);
        scope.extend_from_slice(linked.roots());
        let (langs, lang_by_key) = keys_of(langs);
        Ok(Index { impls, data: IndexData::Owned(IndexOwned { langs, lang_by_key, scope }) })
    }

    /// What the lang rows are a function of: the program's roots (or, from records, the lang
    /// crates given), sorted.
    pub fn scope(&self) -> &[usize] {
        match &self.data {
            IndexData::Owned(owned) => &owned.scope,
            IndexData::Borrowed(shared) => &shared.scope,
        }
    }

    /// The lang items the crates `lang_crates` declare, from any source of names, and no impl
    /// (`impls` stays empty: over records an impl is filed by `buckets::Buckets::lazy` when a
    /// question's home admits its crate, never listed up front). A proc-macro unit's lang rows
    /// are skipped as [`Index::new`] skips the unit. Crates are read in the order given, so
    /// which of two clashing declarations is `first` is a function of that order alone.
    pub fn from_source<S: super::source::NamesSource<'n>>(names: S, lang_crates: &[usize]) -> Result<Index<'n>, LangTwice> {
        let langs = collect_source_langs(names, lang_crates, Global)?;
        let mut scope = Vec::with_capacity_in(lang_crates.len(), Global);
        scope.extend_from_slice(lang_crates);
        scope.sort_unstable();
        scope.dedup();
        let (langs, lang_by_key) = keys_of(langs);
        Ok(Index { impls: Default::default(), data: IndexData::Owned(IndexOwned { langs, lang_by_key, scope }) })
    }

    /// The item `#[lang = "name"]` declares.
    pub fn lang(&self, name: &str) -> Option<LibKey> {
        match &self.data {
            IndexData::Owned(owned) => owned.langs.get(name).copied(),
            IndexData::Borrowed(shared) => shared.langs.get(name).copied(),
        }
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

    /// The `#[lang]` name `key` is declared under, if any (O(1) reverse map lookup).
    pub fn lang_name_of(&self, key: LibKey) -> Option<&'n str> {
        match &self.data {
            IndexData::Owned(owned) => owned.lang_by_key.get(&key).copied(),
            IndexData::Borrowed(shared) => shared.lang_by_key.get(&key).map(|s| s.as_str()),
        }
    }

    pub fn lang_count(&self) -> usize {
        match &self.data {
            IndexData::Owned(owned) => owned.langs.len(),
            IndexData::Borrowed(shared) => shared.langs.len(),
        }
    }
}
