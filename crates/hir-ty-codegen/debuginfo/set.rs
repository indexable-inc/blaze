//! The sets of types a debugger is told of, kept so that the sets with a type share what it
//! is told as. Every type a variable's type is made of is in its set, and every set of a type
//! with a `String` in it has the `String`, its `Vec`, its `RawVec` and the rest: a set is a
//! type ([`Node`]) with the sets of the types it is made of, which the sets with each of
//! those share. What a type is told as apart from the types it is made of ([`Told`]) is
//! interned.
//!
//! The database keeps what each type is told as on its own, with the types it is made of by
//! their keys ([`Own`]), and [`resolve`] puts the sets together from that. Types made of each
//! other, as a struct that holds a `Vec` of its own kind is with the `Vec` through the
//! `Vec`'s generic parameter, are put together as one group, in which each names the others
//! by their places ([`Part::Here`]); every group is put together once, for every set it is
//! in. The store keeps a set as a list of [`DebugType`]s that name each other by their
//! places in it ([`TypeSet::to_vec`]).

use std::{fmt, hash::Hash, sync::Arc};

use blaze_hash::{HashMap, HashSet};
use intern::{Interned, Symbol, impl_internable};

use super::{Aggregate, DebugType, Field, Tag, TypeIndex, Variant};

/// A type and every type it is made of: a type of a group of types made of each other, and
/// of the sets of other types. The sets with a type share its set.
#[derive(Clone)]
pub(crate) struct TypeSet {
    group: Arc<[Node]>,
    nth: u32,
}

pub(super) struct Node {
    pub(super) told: Interned<Told>,
    /// The types it is made of, in the order [`Told`] says.
    of: Box<[Part]>,
}

/// A type that a type is made of.
#[derive(Clone)]
enum Part {
    Type(TypeSet),
    /// The type at this place in the group of the one made of it.
    Here(u32),
    /// What a pointer points to past the pointers followed from a variable: the type of this
    /// name, where a debugger is told of one.
    Named(Symbol),
}

/// A type that a type is made of, as [`TypeSet::of`] tells it.
#[derive(Clone)]
pub(super) enum Of<'a> {
    Type(TypeSet),
    /// See [`Part::Named`].
    Named(&'a Symbol),
}

/// What one type is told as, with the types it is made of by their keys: what the database
/// keeps of each type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: holds counts; the cache of types hands out each part's key, once per type on a miss
pub(crate) struct Own<K> {
    pub(super) told: Interned<Told>,
    pub(super) of: Box<[Made<K>]>,
}

/// A type that one of an [`Own`] is made of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: as `Own`
pub(crate) enum Made<K> {
    Key(K),
    /// See [`Part::Named`].
    Named(Symbol),
    /// A type that is no type of the program's, as the vtable of a pointer to a `dyn` is.
    Inline(Box<Own<K>>),
}

/// Where [`resolve`] finds the types of a set, by a key each.
pub(super) trait Found {
    type Key: Clone + Eq + Hash;
    /// What the type of `key` is told as, `None` for one a debugger is told nothing of.
    fn own(&mut self, key: &Self::Key) -> Option<Arc<Own<Self::Key>>>;
    /// The set of `key` put together before, `Some(None)` for a type told nothing of.
    fn set(&mut self, key: &Self::Key) -> Option<Option<TypeSet>>;
    /// Keeps the set of `key` for every set it is in.
    fn keep(&mut self, key: &Self::Key, set: Option<TypeSet>);
}

/// The set of the type of `key`, as `found` has the types.
pub(super) fn resolve<F: Found>(found: &mut F, key: &F::Key) -> Option<TypeSet> {
    // alloc: empty, allocates nothing on a hit; filled once per type no set was kept of
    let (stack, parts) = (Vec::new(), Vec::new());
    let mut search = Search { found, on: HashMap::default(), stack, parts };
    match search.key(key, 0).0 {
        Seen::Done(Some(Part::Type(set))) => Some(set),
        _ => None,
    }
}

/// What [`Search`] found of a type that a type is made of.
enum Seen {
    /// `None` for a type a debugger is told nothing of.
    Done(Option<Part>),
    /// The type at this place on the stack, which is being put together.
    At(usize),
}

/// The types being put together, by the groups of types made of each other that they are
/// in, as Tarjan's algorithm finds them.
struct Search<'a, F: Found> {
    found: &'a mut F,
    /// The place on `stack` of each type there.
    on: HashMap<F::Key, usize>,
    stack: Vec<Pending<F::Key>>,
    /// The parts of the [`Node`] being made, for [`node`].
    parts: Vec<Option<Part>>,
}

struct Pending<K> {
    /// `None` for one of [`Made::Inline`].
    key: Option<K>,
    told: Interned<Told>,
    of: Vec<Seen>,
    /// The lowest place on the stack of a type it is made of, through the types it is made of.
    low: usize,
}

impl<F: Found> Search<'_, F> {
    /// What the type of `key` is, as a part of a type `depth` types inside a variable's, and
    /// the lowest place on the stack it is made of. A type inside more than
    /// [`DebugType::DEEPEST`] others is not told of, as the writer tells of none: a list read
    /// from the store may be made of any number.
    fn key(&mut self, key: &F::Key, depth: u32) -> (Seen, usize) {
        if let Some(&at) = self.on.get(key) {
            return (Seen::At(at), at);
        }
        if let Some(set) = self.found.set(key) {
            return (Seen::Done(set.map(Part::Type)), usize::MAX);
        }
        if depth >= DebugType::DEEPEST {
            return (Seen::Done(None), usize::MAX);
        }
        let Some(own) = self.found.own(key) else {
            self.found.keep(key, None);
            return (Seen::Done(None), usize::MAX);
        };
        // clone: a key is counts, held by the map and the stack; once per type put together
        self.on.insert(key.clone(), self.stack.len());
        // clone: as above
        self.own(Some(key.clone()), &own, depth)
    }

    fn own(&mut self, key: Option<F::Key>, own: &Own<F::Key>, depth: u32) -> (Seen, usize) {
        let at = self.stack.len();
        let of = Vec::with_capacity(own.of.len());
        // clone: the interned type's count, once per type put together
        self.stack.push(Pending { key, told: own.told.clone(), of, low: at });
        for made in &own.of {
            let (seen, low) = match made {
                Made::Key(key) => self.key(key, depth + 1),
                // clone: the name's count, kept in the node
                Made::Named(name) => (Seen::Done(Some(Part::Named(name.clone()))), usize::MAX),
                Made::Inline(own) => self.own(None, own, depth + 1),
            };
            let pending = &mut self.stack[at];
            pending.low = pending.low.min(low);
            pending.of.push(seen);
        }
        let low = self.stack[at].low;
        if low < at {
            return (Seen::At(at), low);
        }
        // The type and the ones after it on the stack are made of each other, and of no type
        // before it.
        for key in self.stack[at..].iter().filter_map(|it| it.key.as_ref()) {
            self.on.remove(key);
        }
        // An array of a type that is not told of is not told of either, and is a group of its
        // own: it is made of nothing else.
        if let [array] = &self.stack[at..]
            && let Told::Array { .. } = &*array.told
            && !matches!(array.of.first(), Some(Seen::Done(Some(Part::Type(_)))))
        {
            if let Some(key) = &array.key {
                self.found.keep(key, None);
            }
            self.stack.truncate(at);
            return (Seen::Done(None), usize::MAX);
        }
        let parts = &mut self.parts;
        let nodes = self.stack[at..].iter_mut().map(|it| {
            parts.extend(it.of.drain(..).map(|seen| match seen {
                Seen::At(on) => Some(Part::Here((on - at) as u32)),
                Seen::Done(part) => part,
            }));
            node(&it.told, parts)
        });
        // alloc: the group's nodes, kept as the sets of its types; once per group
        let nodes: Arc<[Node]> = nodes.collect();
        for (nth, it) in self.stack[at..].iter().enumerate() {
            if let Some(key) = &it.key {
                // clone: the group's count, one for each of its types kept
                let set = TypeSet { group: nodes.clone(), nth: nth as u32 };
                self.found.keep(key, Some(set));
            }
        }
        self.stack.truncate(at);
        (Seen::Done(Some(Part::Type(TypeSet { group: nodes, nth: 0 }))), usize::MAX)
    }
}

/// `told`, made of the types `of`: without the fields and the parameters of a type that is
/// not told of (`None`), nor what a pointer points to then, as the list the store keeps has
/// it ([`expand`]). Leaves `of` empty.
fn node(told: &Interned<Told>, of: &mut Vec<Option<Part>>) -> Node {
    if of.iter().all(Option::is_some) {
        // clone: the interned type's count; alloc: the node's parts, kept in the set
        return Node { told: told.clone(), of: of.drain(..).flatten().collect() };
    }
    fn kept<T: Clone>(
        items: &[T],
        parts: &mut impl Iterator<Item = Option<Part>>,
        kept: &mut Vec<Part>,
    ) -> Box<[T]> {
        let told = |_: &&T| match parts.next().flatten() {
            Some(part) => {
                kept.push(part);
                true
            }
            None => false,
        };
        // alloc: what is told of the items, interned with the type
        items.iter().filter(told).cloned().collect()
    }
    fn reshaped(
        it: &Shape,
        parts: &mut impl Iterator<Item = Option<Part>>,
        of: &mut Vec<Part>,
    ) -> Shape {
        Shape {
            // clone: the name's count, interned with the type
            name: it.name.clone(),
            size: it.size,
            fields: kept(&it.fields, parts, of),
            parameters: kept(&it.parameters, parts, of),
        }
    }
    let mut parts = of.drain(..);
    // An array of a type not told of keeps none of its parts.
    let keeps = if let Told::Array { .. } = &**told { 0 } else { parts.len() };
    let mut of = Vec::with_capacity(keeps);
    let told = match &**told {
        Told::Pointer { name } => {
            of.extend(parts.flatten());
            // clone: the name's count, interned with the type
            Told::Pointer { name: name.clone() }
        }
        Told::Struct(it) => Told::Struct(reshaped(it, &mut parts, &mut of)),
        Told::Union(it) => Told::Union(reshaped(it, &mut parts, &mut of)),
        Told::Enum { of: it, tag, variants } => {
            let it = reshaped(it, &mut parts, &mut of);
            let variants = variants
                .iter()
                .map(|variant| Shaped {
                    // clone: the name's count, interned with the type
                    name: variant.name.clone(),
                    tag: variant.tag,
                    fields: kept(&variant.fields, &mut parts, &mut of),
                })
                // alloc: the variants, interned with the type
                .collect();
            Told::Enum { of: it, tag: *tag, variants }
        }
        // An array of a type not told of is not told of ([`Search::own`]), and these are
        // made of no type.
        Told::Array { .. } | Told::Base { .. } | Told::Names { .. } => {
            // clone: the interned type's count; alloc: empty, allocates nothing
            return Node { told: told.clone(), of: Box::new([]) };
        }
    };
    Node { told: Interned::new(told), of: of.into() }
}

/// What a debugger is told of a type apart from where in its set the types it is made of are,
/// which the type is made of in the order [`TypeSet::get`] tells.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) enum Told {
    Base {
        name: Symbol,
        size: u64,
        encoding: u8,
    },
    /// Made of what it points to where that is told.
    Pointer {
        name: Symbol,
    },
    /// Made of its element.
    Array {
        len: u64,
    },
    /// Made of the types of its fields, then of what its generic parameters stand for.
    Struct(Shape),
    Union(Shape),
    Names {
        name: Symbol,
        size: u64,
        names: Box<[(Symbol, u64)]>,
    },
    /// Made of what [`Told::Struct`] is, then of the types of the fields of each variant.
    Enum {
        of: Shape,
        tag: Option<Tag>,
        variants: Box<[Shaped]>,
    },
}

impl_internable!(Told);

impl Told {
    /// The name a debugger knows the type by, where it has one.
    fn name(&self) -> Option<&Symbol> {
        match self {
            Told::Base { name, .. } | Told::Pointer { name } | Told::Names { name, .. } => {
                Some(name)
            }
            Told::Struct(it) | Told::Union(it) | Told::Enum { of: it, .. } => Some(&it.name),
            Told::Array { .. } => None,
        }
    }
}

/// An [`Aggregate`] without the places of its types.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Shape {
    pub(super) name: Symbol,
    pub(super) size: u64,
    /// The name and the offset of each field.
    pub(super) fields: Box<[(Symbol, u64)]>,
    pub(super) parameters: Box<[Symbol]>,
}

/// A [`Variant`] without the places of the types of its fields.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Shaped {
    pub(super) name: Symbol,
    pub(super) tag: Option<u64>,
    pub(super) fields: Box<[(Symbol, u64)]>,
}

impl TypeSet {
    /// What tells the set from every other one there is while it is.
    pub(crate) fn at(&self) -> usize {
        std::ptr::from_ref(self.node()).addr()
    }

    pub(super) fn node(&self) -> &Node {
        &self.group[self.nth as usize]
    }

    /// The types it is made of, in the order [`Told`] says.
    pub(super) fn of(&self) -> impl Iterator<Item = Of<'_>> {
        self.node().of.iter().map(|part| match part {
            // clone: a group's count, as `Of` (named in `debuginfo.rs`) holds a set owned
            Part::Type(set) => Of::Type(set.clone()),
            // clone: as above, of this set's group
            Part::Here(nth) => Of::Type(TypeSet { group: self.group.clone(), nth: *nth }),
            Part::Named(name) => Of::Named(name),
        })
    }

    /// The set as a list of types, the type itself first, which is how the store keeps it.
    pub(crate) fn to_vec(&self) -> Vec<DebugType> {
        let mut flat = Flat::default();
        flat.add(self);
        let Flat { mut types, names, named, .. } = flat;
        for (at, nth, name) in named {
            types[at as usize].1[nth] = names.get(&name).copied();
        }
        // alloc: the list, once per set listed
        types.iter().map(|(told, of)| expand(told, of)).collect()
    }

    /// The set of the first of `types`, which name each other by their places among them.
    fn of_list(types: &[DebugType]) -> TypeSet {
        let mut listed = Listed { types, sets: HashMap::default() };
        resolve(&mut listed, &0).unwrap_or_else(|| {
            let told = Told::Struct(Shape {
                name: Symbol::intern("()"),
                size: 0,
                // alloc: empty, allocates nothing
                fields: Box::new([]),
                // alloc: empty, allocates nothing
                parameters: Box::new([]),
            });
            // alloc: empty, allocates nothing
            let node = Node { told: Interned::new(told), of: Box::new([]) };
            // alloc: the set of a list of no type read from the store, once per list read
            TypeSet { group: Arc::new([node]), nth: 0 }
        })
    }
}

impl PartialEq for TypeSet {
    /// Whether the two tell the same: types made of each other are the same where nothing
    /// else tells them apart.
    fn eq(&self, other: &Self) -> bool {
        fn same(one: &TypeSet, other: &TypeSet, assumed: &mut HashSet<(usize, usize)>) -> bool {
            if one.at() == other.at() || !assumed.insert((one.at(), other.at())) {
                return true;
            }
            one.node().told == other.node().told
                && one.node().of.len() == other.node().of.len()
                && one.of().zip(other.of()).all(|pair| match pair {
                    (Of::Type(one), Of::Type(other)) => same(&one, &other, assumed),
                    (Of::Named(one), Of::Named(other)) => one == other,
                    _ => false,
                })
        }
        same(self, other, &mut HashSet::default())
    }
}

impl Eq for TypeSet {}

impl From<&[DebugType]> for TypeSet {
    fn from(types: &[DebugType]) -> Self {
        TypeSet::of_list(types)
    }
}

impl From<Vec<DebugType>> for TypeSet {
    fn from(types: Vec<DebugType>) -> Self {
        TypeSet::of_list(&types)
    }
}

impl fmt::Debug for TypeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // alloc: debug dump only
        f.debug_list().entries(self.to_vec()).finish()
    }
}

/// A set being written as a list ([`TypeSet::to_vec`]).
#[derive(Default)]
struct Flat {
    /// Each type of the list, with the places of the types it is made of: `None` for a name
    /// not looked up yet.
    types: Vec<(Interned<Told>, Vec<Option<TypeIndex>>)>,
    /// The place of each type already in the list, by where it is.
    places: HashMap<usize, TypeIndex>,
    /// The first type of each name in the list.
    names: HashMap<Symbol, TypeIndex>,
    /// The [`Part::Named`]s, by the type and the part, to look up once all are in.
    named: Vec<(TypeIndex, usize, Symbol)>,
}

impl Flat {
    fn add(&mut self, set: &TypeSet) -> TypeIndex {
        if let Some(&at) = self.places.get(&set.at()) {
            return at;
        }
        let at = self.types.len() as TypeIndex;
        // clone: the interned type's count, once per type of a set written to the store
        let told = set.node().told.clone();
        if let Some(name) = told.name() {
            // clone: the name's count, as above
            self.names.entry(name.clone()).or_insert(at);
        }
        // alloc: empty, allocates nothing; the places are put in below
        self.types.push((told, Vec::new()));
        self.places.insert(set.at(), at);
        let mut of = Vec::with_capacity(set.node().of.len());
        for (nth, part) in set.of().enumerate() {
            of.push(match part {
                Of::Type(set) => Some(self.add(&set)),
                Of::Named(name) => {
                    // clone: the name's count, once per name of a set written to the store
                    self.named.push((at, nth, name.clone()));
                    None
                }
            });
        }
        self.types[at as usize].1 = of;
        at
    }
}

/// A list of types read from the store, for [`resolve`] to put a set together from.
struct Listed<'a> {
    types: &'a [DebugType],
    sets: HashMap<TypeIndex, Option<TypeSet>>,
}

impl Found for Listed<'_> {
    type Key = TypeIndex;

    fn own(&mut self, key: &TypeIndex) -> Option<Arc<Own<TypeIndex>>> {
        // alloc: once per type of a list read
        let mut of = Vec::new();
        let told = Interned::new(shape(self.types.get(*key as usize)?, &mut of));
        // alloc: as above, `Found::own` answers an `Own` counted
        Some(Arc::new(Own { told, of: of.into_iter().map(Made::Key).collect() }))
    }

    fn set(&mut self, key: &TypeIndex) -> Option<Option<TypeSet>> {
        self.sets.get(key).cloned()
    }

    fn keep(&mut self, key: &TypeIndex, set: Option<TypeSet>) {
        self.sets.insert(*key, set);
    }
}

/// What `ty` is told as, with the places of the types it is made of pushed to `of`.
pub(super) fn shape(ty: &DebugType, of: &mut Vec<TypeIndex>) -> Told {
    let names = |fields: &[Field], of: &mut Vec<TypeIndex>| -> Box<[(Symbol, u64)]> {
        of.extend(fields.iter().map(|it| it.ty));
        // alloc: what is told of the fields, interned; once per type on a miss or a read
        fields.iter().map(|it| (Symbol::intern(&it.name), it.offset)).collect()
    };
    let aggregate = |it: &Aggregate, of: &mut Vec<TypeIndex>| {
        let fields = names(&it.fields, of);
        of.extend(it.parameters.iter().map(|(_, ty)| *ty));
        // alloc: as above, of the parameters
        let parameters = it.parameters.iter().map(|(name, _)| Symbol::intern(name)).collect();
        Shape { name: Symbol::intern(&it.name), size: it.size, fields, parameters }
    };
    match ty {
        DebugType::Base { name, size, encoding } => {
            Told::Base { name: Symbol::intern(name), size: *size, encoding: *encoding }
        }
        DebugType::Pointer { name, to } => {
            of.extend(*to);
            Told::Pointer { name: Symbol::intern(name) }
        }
        DebugType::Array { of: element, len } => {
            of.push(*element);
            Told::Array { len: *len }
        }
        DebugType::Struct(it) => Told::Struct(aggregate(it, of)),
        DebugType::Union(it) => Told::Union(aggregate(it, of)),
        DebugType::Names { name, size, names } => Told::Names {
            name: Symbol::intern(name),
            size: *size,
            // alloc: what is told of the values, interned; once per type on a miss or a read
            names: names.iter().map(|(name, value)| (Symbol::intern(name), *value)).collect(),
        },
        DebugType::Enum { of: it, tag, variants } => {
            let it = aggregate(it, of);
            let variants = variants
                .iter()
                .map(|it| Shaped {
                    name: Symbol::intern(&it.name),
                    tag: it.tag,
                    fields: names(&it.fields, of),
                })
                // alloc: as above, of the variants
                .collect();
            Told::Enum { of: it, tag: *tag, variants }
        }
    }
}

/// The type `told` is, made of the types at `of`: one at `None` is left out.
fn expand(told: &Told, of: &[Option<TypeIndex>]) -> DebugType {
    type Places<'a> = std::iter::Copied<std::slice::Iter<'a, Option<TypeIndex>>>;
    fn fields(names: &[(Symbol, u64)], of: &mut Places<'_>) -> Vec<Field> {
        let fields = names.iter().zip(of);
        let told = fields.filter_map(|(it, ty)| Some((it, ty?)));
        // alloc: `DebugType`'s names are `String`s; once per type of a set written to the store
        told.map(|((name, offset), ty)| Field { name: name.to_string(), offset: *offset, ty })
            // alloc: as above
            .collect()
    }
    fn aggregate(it: &Shape, of: &mut Places<'_>) -> Aggregate {
        let fields = fields(&it.fields, of);
        let parameters = it.parameters.iter().zip(of).filter_map(|(name, ty)| Some((name, ty?)));
        Aggregate {
            // alloc: `DebugType`'s names are `String`s; once per type of a set written
            name: it.name.to_string(),
            size: it.size,
            fields,
            // alloc: as above
            parameters: parameters.map(|(name, ty)| (name.to_string(), ty)).collect(),
        }
    }
    let mut of = of.iter().copied();
    match told {
        Told::Base { name, size, encoding } => {
            // alloc: `DebugType`'s names are `String`s; once per type of a set written
            DebugType::Base { name: name.to_string(), size: *size, encoding: *encoding }
        }
        Told::Pointer { name } => {
            // alloc: as above
            DebugType::Pointer { name: name.to_string(), to: of.next().flatten() }
        }
        Told::Array { len } => {
            DebugType::Array { of: of.next().flatten().unwrap_or_default(), len: *len }
        }
        Told::Struct(it) => DebugType::Struct(aggregate(it, &mut of)),
        Told::Union(it) => DebugType::Union(aggregate(it, &mut of)),
        Told::Names { name, size, names } => DebugType::Names {
            // alloc: as above
            name: name.to_string(),
            size: *size,
            // alloc: as above
            names: names.iter().map(|(name, value)| (name.to_string(), *value)).collect(),
        },
        Told::Enum { of: it, tag, variants } => {
            let it = aggregate(it, &mut of);
            let variants = variants.iter().map(|variant| {
                let fields = fields(&variant.fields, &mut of);
                // alloc: as above
                Variant { name: variant.name.to_string(), tag: variant.tag, fields }
            });
            // alloc: as above
            DebugType::Enum { of: it, tag: *tag, variants: variants.collect() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, offset: u64, ty: TypeIndex) -> Field {
        Field { name: name.into(), offset, ty }
    }

    fn aggregate(
        name: &str,
        size: u64,
        fields: Vec<Field>,
        parameters: Vec<(String, TypeIndex)>,
    ) -> Aggregate {
        Aggregate { name: name.into(), size, fields, parameters }
    }

    /// `Option<Box<List>>` where a `List` holds a `u8`, the `Option` of the next one and more:
    /// a pointer back into the set, an enum with a tag, a generic parameter and an array, in
    /// the order a set is written in.
    fn list() -> Vec<DebugType> {
        vec![
            DebugType::Enum {
                of: aggregate("Option<Box<List>>", 8, vec![], vec![("T".into(), 1)]),
                tag: Some(Tag { offset: 0, size: 8, states: false }),
                variants: vec![
                    Variant { name: "None".into(), tag: Some(0), fields: vec![] },
                    Variant { name: "Some".into(), tag: None, fields: vec![field("__0", 0, 1)] },
                ],
            },
            DebugType::Pointer { name: "Box<List>".into(), to: Some(2) },
            DebugType::Struct(aggregate(
                "List",
                40,
                vec![
                    field("value", 8, 3),
                    field("next", 0, 0),
                    field("pad", 9, 4),
                    field("raw", 16, 5),
                    field("order", 24, 6),
                    field("slot", 25, 7),
                ],
                vec![],
            )),
            DebugType::Base { name: "u8".into(), size: 1, encoding: 8 },
            DebugType::Array { of: 3, len: 7 },
            DebugType::Pointer { name: "*const ()".into(), to: None },
            DebugType::Names {
                name: "Ordering".into(),
                size: 1,
                names: vec![("Less".into(), 255), ("Equal".into(), 0)],
            },
            DebugType::Union(aggregate(
                "MaybeUninit<u8>",
                1,
                vec![field("value", 0, 3)],
                vec![("T".into(), 3)],
            )),
        ]
    }

    #[test]
    fn a_set_is_what_it_was_made_of() {
        let set = TypeSet::from(list());
        assert_eq!(set.to_vec(), list());
        assert!(TypeSet::from(set.to_vec()) == set);
    }

    /// The type at `places` among the ones `set` is made of, and the ones they are.
    fn part(set: &TypeSet, places: &[usize]) -> TypeSet {
        let mut part = set.clone();
        for &at in places {
            let Some(Of::Type(set)) = part.of().nth(at) else { panic!("no type at {places:?}") };
            part = set;
        }
        part
    }

    #[test]
    fn a_type_in_a_set_twice_is_one() {
        let set = TypeSet::from(list());
        // The `u8` of `List::value`, and of the field and the parameter of `MaybeUninit<u8>`.
        let value = part(&set, &[0, 0, 0]).at();
        assert!(value == part(&set, &[0, 0, 5, 0]).at() && value == part(&set, &[0, 0, 5, 1]).at());
        // `List::next` is the set itself.
        assert_eq!(part(&set, &[0, 0, 1]).at(), set.at());
    }

    /// Types by their names, each made of the ones it names.
    #[derive(Default)]
    struct Named(Vec<(&'static str, Vec<&'static str>)>, HashMap<&'static str, Option<TypeSet>>);

    impl Found for Named {
        type Key = &'static str;

        fn own(&mut self, key: &&'static str) -> Option<Arc<Own<&'static str>>> {
            let (_, of) = self.0.iter().find(|(name, _)| name == key)?;
            let parameters = of.iter().map(|it| Symbol::intern(it)).collect();
            let shape =
                Shape { name: Symbol::intern(key), size: 8, fields: Box::new([]), parameters };
            let told = Interned::new(Told::Struct(shape));
            Some(Arc::new(Own { told, of: of.iter().map(|it| Made::Key(*it)).collect() }))
        }

        fn set(&mut self, key: &&'static str) -> Option<Option<TypeSet>> {
            self.1.get(key).cloned()
        }

        fn keep(&mut self, key: &&'static str, set: Option<TypeSet>) {
            self.1.insert(key, set);
        }
    }

    #[test]
    fn a_type_made_of_itself_through_a_parameter() {
        // `struct Tree { children: Vec<Tree> }`, as the parameter of the `Vec` names it.
        let types = vec![("Tree", vec!["Vec<Tree>", "u8"]), ("Vec<Tree>", vec!["Tree"])];
        let mut found = Named(types, HashMap::default());
        let tree = resolve(&mut found, &"Tree").unwrap();
        assert_eq!(part(&tree, &[0, 0]).at(), tree.at());
        // The `u8`, which nothing tells of, is left out with its parameter.
        assert_eq!(tree.of().count(), 1);
        // The `Vec<Tree>` of every other set is this one's.
        let vec = resolve(&mut found, &"Vec<Tree>").unwrap();
        assert_eq!(vec.at(), part(&tree, &[0]).at());
        let names: Vec<String> = tree
            .to_vec()
            .iter()
            .map(|it| match it {
                DebugType::Struct(it) => format!("{} {:?}", it.name, it.parameters),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["Tree [(\"Vec<Tree>\", 1)]", "Vec<Tree> [(\"Tree\", 0)]"]);
    }

    #[test]
    fn types_made_of_each_other_in_many_ways_are_put_together_once() {
        // Each of 40 types is made of every other one, as the types of a syntax tree are of
        // each other through `Vec`s and `Option`s: a set put together for each way to
        // reach a type would never be done.
        let names: Vec<&'static str> =
            (0..40).map(|at| &*Box::leak(format!("T{at}").into_boxed_str())).collect();
        let types = names.iter().map(|name| (*name, names.clone())).collect();
        let mut found = Named(types, HashMap::default());
        let set = resolve(&mut found, &names[0]).unwrap();
        assert_eq!(set.to_vec().len(), 40);
        assert_eq!(found.1.len(), 40);
    }
}
