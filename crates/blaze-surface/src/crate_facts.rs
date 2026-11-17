//! The records keyed by a crate's roots: [`LangItems`], [`CrateGraph`] and [`NoImplsOf`].

use core::alloc::Allocator;
use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::{ContentId, CrateItemKey, ItemKey};

use crate::column::{
    Column, Strings, Text, check_increasing, check_index, check_text, count, finish_with, text_of,
    wide,
};
use crate::error::BuildError;

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct LangRow: 1 {
        pub(crate) name: Text,
        pub(crate) item: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct LangItemsRoot: 1 {
        pub(crate) krate: ContentId,
        pub(crate) items: Slice<LangRow>,
        pub(crate) strings: Slice<u8>,
    }
}

/// The writer of one crate's [`LangItems`]: each `#[lang = "name"]` it defines. A reader across
/// the graph reads each crate's, which [`CrateGraph`] lists; two of one name is refused.
pub struct LangItems<A: Allocator + Clone> {
    krate: CrateItemKey,
    rows: Vec<LangRow, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> LangItems<A> {
    pub fn new_in(krate: CrateItemKey, alloc: A) -> LangItems<A> {
        LangItems { krate, rows: Vec::new_in(alloc.clone()), staging: Strings::new_in(alloc.clone()), alloc }
    }

    pub fn item(&mut self, name: &str, item: ItemKey) -> Result<(), BuildError> {
        let name = self.staging.push(name, "LangItems strings")?;
        self.rows.push(LangRow { name, item: item.0 });
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        self.rows.sort_unstable_by(|a, b| (staging.of(a.name), a.item).cmp(&(staging.of(b.name), b.item)));
        self.rows.dedup_by(|a, b| a.item == b.item && staging.of(a.name) == staging.of(b.name));
        if let Some(pair) = self.rows.windows(2).find(|pair| staging.of(pair[0].name) == staging.of(pair[1].name)) {
            return Err(BuildError::Twice { table: "LangItems items", key: pair[1].item });
        }
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.rows {
            row.name = strings.moved(&self.staging, row.name, "LangItems strings")?;
        }
        let mut out = Writer::new_in(<LangItemsRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let items = Column::push(&mut out, &self.rows);
        Ok(finish_with(out, |at| LangItemsRoot {
            krate: self.krate.0,
            items: items.slice(at, offset_of!(LangItemsRoot, items)),
            strings: text.slice(at, offset_of!(LangItemsRoot, strings)),
        }))
    }
}

/// A [`LangItems`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct LangItemsView<'a> {
    krate: CrateItemKey,
    rows: &'a [LangRow],
    strings: &'a str,
}

impl<'a> LangItemsView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<LangItemsView<'a>, FormatError> {
        let root = blaze_format::open::<LangItemsRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let rows = root.items.follow(buffer)?;
        for row in rows {
            check_text(strings, row.name, row, buffer)?;
        }
        check_increasing(rows, buffer, "lang items out of order or repeated", |row| text_of(strings, row.name))?;
        Ok(LangItemsView { krate: CrateItemKey(root.krate), rows, strings })
    }

    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    /// The item of `#[lang = "name"]` in this crate, by one binary search.
    pub fn get(&self, name: &str) -> Option<ItemKey> {
        let strings = self.strings;
        let at = self.rows.binary_search_by(|row| text_of(strings, row.name).cmp(name)).ok()?;
        Some(ItemKey(self.rows[at].item))
    }

    pub fn items(&self) -> impl Iterator<Item = (&'a str, ItemKey)> + use<'a> {
        let strings = self.strings;
        self.rows.iter().map(move |row| (text_of(strings, row.name), ItemKey(row.item)))
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct CrateRow: 1 {
        pub(crate) key: ContentId,
        pub(crate) name: Text,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct DepRow: 1 {
        pub(crate) name: Text,
        pub(crate) from: u32,
        pub(crate) to: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct CrateGraphRoot: 1 {
        pub(crate) root: ContentId,
        pub(crate) crates: Slice<CrateRow>,
        pub(crate) deps: Slice<DepRow>,
        pub(crate) strings: Slice<u8>,
    }
}

/// The writer of the [`CrateGraph`] a crate sees: every crate it reaches, each with its name,
/// and each dependency edge under the name its dependent uses. Crates are sorted by key and
/// edges by dependent and name at the end.
pub struct CrateGraph<A: Allocator + Clone> {
    root: CrateItemKey,
    crates: Vec<CrateRow, A>,
    deps: Vec<(ContentId, ContentId, Text), A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> CrateGraph<A> {
    pub fn new_in(root: CrateItemKey, alloc: A) -> CrateGraph<A> {
        CrateGraph {
            root,
            crates: Vec::new_in(alloc.clone()),
            deps: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    /// A crate of the graph, by its key and its package's name.
    pub fn krate(&mut self, krate: CrateItemKey, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "CrateGraph strings")?;
        self.crates.push(CrateRow { key: krate.0, name });
        Ok(())
    }

    /// `from` depends on `to` under the extern name `name`.
    pub fn dep(&mut self, from: CrateItemKey, to: CrateItemKey, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "CrateGraph strings")?;
        self.deps.push((from.0, to.0, name));
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        self.crates.sort_unstable_by_key(|row| row.key);
        if let Some(pair) = self.crates.windows(2).find(|pair| pair[0].key == pair[1].key) {
            return Err(BuildError::Twice { table: "CrateGraph crates", key: pair[1].key });
        }
        let at_of = |key: ContentId| -> Result<u32, BuildError> {
            let at = self
                .crates
                .binary_search_by_key(&key, |row| row.key)
                .map_err(|_| BuildError::Unknown { table: "CrateGraph deps", key })?;
            count(at, "CrateGraph crates")
        };
        let mut deps = Vec::with_capacity_in(self.deps.len(), self.alloc.clone());
        for &(from, to, name) in &self.deps {
            deps.push(DepRow { name, from: at_of(from)?, to: at_of(to)? });
        }
        deps.sort_unstable_by(|a, b| (a.from, staging.of(a.name), a.to).cmp(&(b.from, staging.of(b.name), b.to)));
        deps.dedup_by(|a, b| (a.from, a.to) == (b.from, b.to) && staging.of(a.name) == staging.of(b.name));
        if let Some(pair) =
            deps.windows(2).find(|pair| pair[0].from == pair[1].from && staging.of(pair[0].name) == staging.of(pair[1].name))
        {
            return Err(BuildError::Twice { table: "CrateGraph deps", key: self.crates[wide(pair[1].to)].key });
        }
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.crates {
            row.name = strings.moved(&self.staging, row.name, "CrateGraph strings")?;
        }
        for row in &mut deps {
            row.name = strings.moved(&self.staging, row.name, "CrateGraph strings")?;
        }
        let mut out = Writer::new_in(<CrateGraphRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let crates = Column::push(&mut out, &self.crates);
        let edges = Column::push(&mut out, &deps);
        Ok(finish_with(out, |at| CrateGraphRoot {
            root: self.root.0,
            crates: crates.slice(at, offset_of!(CrateGraphRoot, crates)),
            deps: edges.slice(at, offset_of!(CrateGraphRoot, deps)),
            strings: text.slice(at, offset_of!(CrateGraphRoot, strings)),
        }))
    }
}

/// A [`CrateGraph`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct CrateGraphView<'a> {
    root: CrateItemKey,
    crates: &'a [CrateRow],
    deps: &'a [DepRow],
    strings: &'a str,
}

impl<'a> CrateGraphView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<CrateGraphView<'a>, FormatError> {
        let root = blaze_format::open::<CrateGraphRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let crates = root.crates.follow(buffer)?;
        let deps = root.deps.follow(buffer)?;
        for row in crates {
            check_text(strings, row.name, row, buffer)?;
        }
        for row in deps {
            check_text(strings, row.name, row, buffer)?;
            for index in [row.from, row.to] {
                check_index(index, crates.len(), row, buffer, "an edge of a crate the graph does not hold")?;
                if index == crate::column::NONE {
                    return Err(FormatError::Invalid {
                        at: crate::column::at_of_record(row, buffer),
                        what: "an edge of no crate",
                    });
                }
            }
        }
        check_increasing(crates, buffer, "crates out of order or repeated", |row| row.key)?;
        check_increasing(deps, buffer, "edges out of order or two of one name", |row| {
            (row.from, text_of(strings, row.name))
        })?;
        Ok(CrateGraphView { root: CrateItemKey(root.root), crates, deps, strings })
    }

    /// The crate whose graph this is.
    pub fn root(&self) -> CrateItemKey {
        self.root
    }

    /// Every crate, by key, with its name.
    pub fn crates(&self) -> impl Iterator<Item = (CrateItemKey, &'a str)> + use<'a> {
        let strings = self.strings;
        self.crates.iter().map(move |row| (CrateItemKey(row.key), text_of(strings, row.name)))
    }

    /// The dependencies of `krate`, by extern name.
    pub fn deps_of(&self, krate: CrateItemKey) -> impl Iterator<Item = (&'a str, CrateItemKey)> + use<'a> {
        let (crates, strings) = (self.crates, self.strings);
        let from = crates.binary_search_by_key(&krate.0, |row| row.key).ok();
        let range = match from.and_then(|from| u32::try_from(from).ok()) {
            Some(from) => {
                self.deps.partition_point(|row| row.from < from)..self.deps.partition_point(|row| row.from <= from)
            }
            None => 0..0,
        };
        self.deps[range]
            .iter()
            .map(move |row| (text_of(strings, row.name), CrateItemKey(crates[wide(row.to)].key)))
    }

    /// The crate `krate` names `name`, if it depends on one so.
    pub fn dep(&self, krate: CrateItemKey, name: &str) -> Option<CrateItemKey> {
        self.deps_of(krate).find(|(dep, _)| *dep == name).map(|(_, key)| key)
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct NoImplsOfRoot: 1 {
        pub(crate) krate: ContentId,
        pub(crate) traits: Slice<ContentId>,
    }
}

/// The writer of a crate's [`NoImplsOf`]: the traits it has at least one impl of, so a trait
/// absent here is one it has none of, and an impl lookup does not probe its buckets.
pub struct NoImplsOf<A: Allocator + Clone> {
    krate: CrateItemKey,
    traits: Vec<ContentId, A>,
    alloc: A,
}

impl<A: Allocator + Clone> NoImplsOf<A> {
    pub fn new_in(krate: CrateItemKey, alloc: A) -> NoImplsOf<A> {
        NoImplsOf { krate, traits: Vec::new_in(alloc.clone()), alloc }
    }

    /// The crate has an impl of `trait_`.
    pub fn implements(&mut self, trait_: ItemKey) {
        self.traits.push(trait_.0);
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        self.traits.sort_unstable();
        self.traits.dedup();
        count(self.traits.len(), "NoImplsOf traits")?;
        let mut out = Writer::new_in(<NoImplsOfRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let traits = Column::push(&mut out, &self.traits);
        Ok(finish_with(out, |at| NoImplsOfRoot {
            krate: self.krate.0,
            traits: traits.slice(at, offset_of!(NoImplsOfRoot, traits)),
        }))
    }
}

/// A [`NoImplsOf`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct NoImplsOfView<'a> {
    krate: CrateItemKey,
    traits: &'a [ContentId],
}

impl<'a> NoImplsOfView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<NoImplsOfView<'a>, FormatError> {
        let root = blaze_format::open::<NoImplsOfRoot>(buffer)?;
        let traits = root.traits.follow(buffer)?;
        check_increasing(traits, buffer, "traits out of order or repeated", |key| *key)?;
        Ok(NoImplsOfView { krate: CrateItemKey(root.krate), traits })
    }

    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    /// Whether the crate has no impl of `trait_`, by one binary search.
    pub fn has_no_impl_of(&self, trait_: ItemKey) -> bool {
        self.traits.binary_search(&trait_.0).is_err()
    }
}
