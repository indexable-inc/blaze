//! `profile.<name>` of the configuration, read by the manifest's own profile reader
//! ([`TomlProfile::read`]) as cargo's `get_config_profile` reads it with the manifest's serde
//! type: the configuration's deserializer (which merges the files, `--config` and the
//! `CARGO_PROFILE_*` variables) fills a tree of values, each with where it was defined, and the
//! tree is the [`Node`] the reader walks.
//!
//! The deserializer finds a struct's variables only by its field names, so the tree is read
//! with the fields of a profile where a profile stands (the table itself, `build-override`,
//! each `package.<spec>`) and `rustflags` as a list, which a variable gives split at spaces.

use std::alloc::Global;
use std::fmt;
use std::ops::Range;
use std::ptr;

use blaze_data::{Int, Map, Value as Data};
use serde::de::{self, MapAccess, SeqAccess, Unexpected, Visitor};

use super::error::ConfigError;
use super::value::{Definition, OptValue, Value};
use crate::schema::{Node, Shape, TomlProfile};

/// The keys of a profile table, as cargo-util-schemas' `TomlProfile` names its fields; the
/// keys [`TomlProfile::read`] reads.
const PROFILE_FIELDS: &[&str] = &[
    "opt-level",
    "lto",
    "codegen-backend",
    "codegen-units",
    "debug",
    "split-debuginfo",
    "debug-assertions",
    "rpath",
    "panic",
    "overflow-checks",
    "incremental",
    "dir-name",
    "inherits",
    "strip",
    "rustflags",
    "package",
    "build-override",
    "trim-paths",
    "hint-mostly-unused",
];

/// `profile.<name>` (the configuration key `key`) as the manifest reads a profile.
pub(super) fn read(
    table: OptValue<Profile>,
    key: &str,
) -> Result<OptValue<TomlProfile>, ConfigError> {
    let Some(Value { val: Profile(shape), definition }) = table else { return Ok(None) };
    let root = Tree { shape, definition };
    let at = At { prefix: key, root: &root, tree: &root };
    match TomlProfile::read(at) {
        Ok(val) => Ok(Some(Value { val, definition: root.definition.clone() })),
        Err(error) => {
            // The refused value is the one whose key path the error names; its definition is
            // where cargo says the error is.
            let definition = at.find(&error.key).map(|it| it.definition.clone());
            Err(ConfigError::schema(error, definition))
        }
    }
}

/// A value of the tree: what it is and where it was defined.
struct Tree {
    shape: Shaped,
    definition: Definition,
}

enum Shaped {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Tree>),
    /// A table's entries in the order the deserializer gave them.
    Map(Vec<(String, Tree)>),
}

impl<T: Into<Shaped>> From<Value<T>> for Tree {
    fn from(value: Value<T>) -> Tree {
        Tree { shape: value.val.into(), definition: value.definition }
    }
}

/// How a value is asked of the deserializer: by what it is (`Any`), as a profile's fields, as
/// a map of profiles (`package`), or as a list.
#[derive(Clone, Copy)]
enum Reads {
    Any,
    Profile,
    Packages,
    List,
}

/// The tree's value at a place, with what the reader needs to name the place.
#[derive(Clone, Copy)]
struct At<'n> {
    /// The configuration key of the root (`profile.dev`).
    prefix: &'n str,
    root: &'n Tree,
    tree: &'n Tree,
}

impl<'n> At<'n> {
    fn at(self, tree: &'n Tree) -> At<'n> {
        At { prefix: self.prefix, root: self.root, tree }
    }

    /// The value whose [`Node::path`] is `key`.
    fn find(self, key: &str) -> Option<&'n Tree> {
        if self.path() == key {
            return Some(self.tree);
        }
        let tree: &'n Tree = self.tree;
        match &tree.shape {
            Shaped::Map(entries) => entries.iter().find_map(|(_, it)| self.at(it).find(key)),
            Shaped::List(items) => items.iter().find_map(|it| self.at(it).find(key)),
            Shaped::Str(_) | Shaped::Int(_) | Shaped::Bool(_) => None,
        }
    }
}

/// Appends to `out` the steps from `at` down to `target`; false, with `out` as it was, where
/// `target` is not under `at`.
fn steps(at: &Tree, target: &Tree, out: &mut String) -> bool {
    if ptr::eq(at, target) {
        return true;
    }
    let len = out.len();
    match &at.shape {
        Shaped::Map(entries) => {
            for (key, it) in entries {
                out.push('.');
                out.push_str(key);
                if steps(it, target, out) {
                    return true;
                }
                out.truncate(len);
            }
        }
        Shaped::List(items) => {
            for (i, it) in items.iter().enumerate() {
                out.push_str(&format!("[{i}]"));
                if steps(it, target, out) {
                    return true;
                }
                out.truncate(len);
            }
        }
        Shaped::Str(_) | Shaped::Int(_) | Shaped::Bool(_) => {}
    }
    false
}

impl<'n> Node<'n> for At<'n> {
    fn shape(self) -> Shape<'n> {
        let tree: &'n Tree = self.tree;
        match &tree.shape {
            Shaped::Str(s) => Shape::Str(s.as_str()),
            Shaped::Int(i) => Shape::Int(*i),
            Shaped::Bool(b) => Shape::Bool(*b),
            Shaped::List(_) => Shape::Seq,
            Shaped::Map(_) => Shape::Map,
        }
    }

    fn entries(self) -> impl Iterator<Item = (&'n str, Self)> {
        let tree: &'n Tree = self.tree;
        let entries: &'n [(String, Tree)] = match &tree.shape {
            Shaped::Map(entries) => entries,
            _ => &[],
        };
        entries.iter().map(move |(key, it)| (key.as_str(), self.at(it)))
    }

    fn elements(self) -> impl Iterator<Item = Self> {
        let tree: &'n Tree = self.tree;
        let items: &'n [Tree] = match &tree.shape {
            Shaped::List(items) => items,
            _ => &[],
        };
        items.iter().map(move |it| self.at(it))
    }

    fn get(self, key: &str) -> Option<Self> {
        self.entries().find(|(k, _)| *k == key).map(|(_, it)| it)
    }

    /// A configuration value has no one file; errors name it by its key path and definition
    /// ([`At::find`]) instead.
    fn span(self) -> Range<usize> {
        0..0
    }

    fn key_span(self) -> Range<usize> {
        0..0
    }

    fn path(self) -> String {
        let mut out = self.prefix.to_owned();
        steps(self.root, self.tree, &mut out);
        out
    }

    fn to_value(self) -> Data<Global> {
        fn data(tree: &Tree) -> Data<Global> {
            match &tree.shape {
                Shaped::Str(s) => Data::Text(blaze_own::String::from_str_in(s, Global)),
                Shaped::Int(i) => Data::Int(Int::from_i64(*i)),
                Shaped::Bool(b) => Data::Bool(*b),
                Shaped::List(items) => {
                    let mut out = blaze_own::Vec::with_capacity_in(items.len(), Global);
                    for it in items {
                        out.push(data(it));
                    }
                    Data::List(out)
                }
                Shaped::Map(entries) => {
                    let mut out = Map::new_in(Global);
                    for (key, it) in entries {
                        out.insert(blaze_own::String::from_str_in(key, Global), data(it));
                    }
                    Data::Map(out)
                }
            }
        }
        data(self.tree)
    }

    /// The configuration has no datetimes: a TOML one is refused when its file is read.
    fn datetime_text(self) -> String {
        String::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Reading the tree from the configuration's deserializer.

macro_rules! read_as {
    ($(#[$doc:meta])* $name:ident, $reads:expr, $method:ident $(($($arg:expr),*))?) => {
        $(#[$doc])*
        pub(super) struct $name(Shaped);

        impl From<$name> for Shaped {
            fn from(it: $name) -> Shaped {
                it.0
            }
        }

        impl<'de> de::Deserialize<'de> for $name {
            fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                d.$method($($($arg,)*)? ShapeVisitor($reads)).map($name)
            }
        }
    };
}

read_as!(
    /// A value read by what it is.
    Any, Reads::Any, deserialize_any
);
read_as!(
    /// A profile table, read by its fields.
    Profile, Reads::Profile, deserialize_struct("TomlProfile", PROFILE_FIELDS)
);
read_as!(
    /// `package`: a profile for each package spec.
    Packages, Reads::Packages, deserialize_map
);
read_as!(
    /// `rustflags`.
    List, Reads::List, deserialize_seq
);

struct ShapeVisitor(Reads);

impl<'de> Visitor<'de> for ShapeVisitor {
    type Value = Shaped;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Reads::Any => "a string, integer, boolean, array or table",
            Reads::Profile => "struct TomlProfile",
            Reads::Packages => "a map",
            Reads::List => "a sequence",
        })
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Shaped, E> {
        Ok(Shaped::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Shaped, E> {
        Ok(Shaped::Int(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Shaped, E> {
        let refused = |_: std::num::TryFromIntError| E::invalid_value(Unexpected::Unsigned(v), &self);
        i64::try_from(v).map(Shaped::Int).map_err(refused)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Shaped, E> {
        Ok(Shaped::Str(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Shaped, E> {
        Ok(Shaped::Str(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Shaped, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<Value<Any>>()? {
            items.push(Tree::from(item));
        }
        Ok(Shaped::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shaped, A::Error> {
        let mut entries = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let tree = match (self.0, key.as_str()) {
                (Reads::Profile, "build-override") | (Reads::Packages, _) => {
                    Tree::from(map.next_value::<Value<Profile>>()?)
                }
                (Reads::Profile, "package") => Tree::from(map.next_value::<Value<Packages>>()?),
                (Reads::Profile, "rustflags") => Tree::from(map.next_value::<Value<List>>()?),
                _ => Tree::from(map.next_value::<Value<Any>>()?),
            };
            entries.push((key, tree));
        }
        Ok(Shaped::Map(entries))
    }
}
