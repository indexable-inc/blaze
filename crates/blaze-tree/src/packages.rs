//! The resolve's packages, interned once to a dense [`Pkg`] in `PackageId` order, with what the
//! tree reads of each.

use blaze_hash::{FixedState, HashMap};
use blaze_resolve::{InternedString, PackageId, PackageIdSpec, Resolve};

use blaze_ids::bits::BitSet as Bits;

/// A package of the resolve, by its place in `PackageId` order, so that comparing two compares
/// their ids.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Pkg(u32);

impl Pkg {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// What was read of every package of the resolve: blaze-metadata's, or the database's load.
#[derive(Clone, Copy)]
pub(crate) enum Reads<'a> {
    Metadata(&'a HashMap<PackageId, blaze_metadata::Read>),
    Units(&'a HashMap<PackageId, blaze_units::Read>),
}

/// What the tree reads of one package.
#[derive(Clone, Copy)]
pub(crate) struct Info<'a> {
    pub(crate) any_proc_macro: bool,
    pub(crate) proc_macro_lib: bool,
    pub(crate) lib: Option<InternedString>,
    pub(crate) license: Option<&'a str>,
    pub(crate) repository: Option<&'a str>,
}

macro_rules! info {
    ($read:expr) => {{
        let read = $read;
        Info {
            any_proc_macro: read.any_proc_macro,
            proc_macro_lib: read.proc_macro_lib,
            lib: read.lib,
            license: read.license.as_deref(),
            repository: read.repository.as_deref(),
        }
    }};
}

impl<'a> Reads<'a> {
    pub(crate) fn get(self, id: PackageId) -> Option<Info<'a>> {
        match self {
            Reads::Metadata(reads) => reads.get(&id).map(|read| info!(read)),
            Reads::Units(reads) => reads.get(&id).map(|read| info!(read)),
        }
    }
}

/// The resolve's packages, each hashed once here and indexed by [`Pkg`] after.
pub(crate) struct Table<'a> {
    ids: Vec<PackageId>,
    index: HashMap<PackageId, Pkg>,
    info: Vec<Option<Info<'a>>>,
}

impl<'a> Table<'a> {
    pub(crate) fn new(resolve: &Resolve, reads: Reads<'a>) -> Table<'a> {
        let len = resolve.len();
        let mut ids = Vec::with_capacity(len);
        ids.extend(resolve.iter());
        // The resolve keeps its packages in an ordered map, so this only checks the order.
        ids.sort_unstable();
        let mut index = HashMap::with_capacity_and_hasher(len, FixedState);
        let mut info = Vec::with_capacity(len);
        for (i, &id) in ids.iter().enumerate() {
            let pkg = Pkg(u32::try_from(i).expect("fewer than 2^32 packages"));
            index.entry(id).or_insert(pkg);
            info.push(reads.get(id));
        }
        Table { ids, index, info }
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    /// Every package of the resolve, in order.
    pub(crate) fn ids(&self) -> &[PackageId] {
        &self.ids
    }

    pub(crate) fn id(&self, pkg: Pkg) -> PackageId {
        self.ids[pkg.index()]
    }

    pub(crate) fn pkg(&self, id: PackageId) -> Pkg {
        match self.index.get(&id) {
            Some(&pkg) => pkg,
            None => panic!("{id} is not a package of the resolve"),
        }
    }

    /// What was read of `pkg`, if it was.
    pub(crate) fn read(&self, pkg: Pkg) -> Option<&Info<'a>> {
        self.info[pkg.index()].as_ref()
    }

    /// What was read of `pkg`, which every package the tree shows has.
    pub(crate) fn info(&self, pkg: Pkg) -> &Info<'a> {
        match self.read(pkg) {
            Some(info) => info,
            None => panic!("{} of the resolve was not read", self.id(pkg)),
        }
    }

    pub(crate) fn any_proc_macro(&self, pkg: Pkg) -> bool {
        self.info(pkg).any_proc_macro
    }

    /// The packages any of `specs` matches, each spec matched against each package once.
    pub(crate) fn matching(&self, specs: &[PackageIdSpec]) -> Bits {
        let mut bits = Bits::new(self.len());
        for (i, &id) in self.ids.iter().enumerate() {
            if specs.iter().any(|spec| spec.matches(id)) {
                bits.insert_growing(i);
            }
        }
        bits
    }

    /// The packages of `ids`, leaving out those not of the resolve.
    pub(crate) fn of(&self, ids: &[PackageId]) -> Bits {
        let mut bits = Bits::new(self.len());
        for id in ids {
            if let Some(pkg) = self.index.get(id) {
                bits.insert_growing(pkg.index());
            }
        }
        bits
    }
}
