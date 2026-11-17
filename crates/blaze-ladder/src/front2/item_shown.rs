//! The one printer of an item in a refusal (rule 19: an error names the thing, by its PATH).
//!
//! A key carries no name (rules 21 and 25: a key holds no machine data and no text), and its
//! short `Display` is its CRATE's part (`blaze_id::ItemKey`: the first 12 hex digits are the
//! crate prefix), so a refusal that prints a key names the crate, never the item: on the hello
//! copy users saw "ItemKey(c41ed2ee1fe2)" for a library key and a 38-digit number for a
//! checker key. So a refusal names its item by the path the printing layer resolves from the
//! names it already holds (the names source and the key map), with the key's ITEM part as a
//! short suffix tools can grep: [`ItemShown`].
//!
//! The path is built by a fixed number of steps (an owner, then the item), never by recursion:
//! a member's (an associated fn, const or type) is its trait's or impl's path and its name; a
//! variant's is its enum's path and its name; an impl, which has no name, is shown by its
//! header as written (`<impl Default for stat>`); an item whose module has no path (a block's
//! module) says so instead of being cut to its bare name.

use core::fmt;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};

use blaze_id::ItemKey as LibKey;
use blaze_typeck::ItemKey;

use super::check::Records;
use super::heads::{HeadError, Want};
use super::keys::{Keys, Origin};
use super::source::NamesSource;

/// The digits of a key shown after a path: bytes 8..14, the item's part (bytes 0..8 are the
/// crate's prefix, which the path already names).
const TAIL: core::ops::Range<usize> = 8..14;

/// An item for a refusal: its whole path when the names source gives one, and its key.
/// `Display`: `a::b::Item (key 1a2b3c4d5e6f)`; with no path, every digit of the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemShown {
    /// The item's path, whole; `None`: the names source holds none for the key.
    path: Option<Box<str>>,
    /// The key's first 16 bytes (a checker key is all of them, a library key's first 16).
    key: [u8; 16],
}

impl ItemShown {
    /// A checker key `k` over the key map `keys` (which files members) and `names`.
    pub fn of<'n, S: NamesSource<'n>>(names: S, keys: &Keys, k: ItemKey) -> ItemShown {
        let path = match keys.origin(k) {
            Some(Origin::Member { container, name, .. }) => {
                let owner = keys.library_in(names, *container).and_then(|c| lib_path(names, c));
                Some(match owner {
                    Some(o) => format!("{o}::{name}"),
                    None => format!("<a container the names source holds no path for>::{name}"),
                })
            }
            _ => keys.library_in(names, k).and_then(|lib| lib_path(names, lib)),
        };
        ItemShown { path: path.map(String::into_boxed_str), key: k.0.to_be_bytes() }
    }

    /// A library key `lib` over `names`.
    pub fn of_lib<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> ItemShown {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, ..] = lib.0.0;
        let key = [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15];
        ItemShown { path: lib_path(names, lib).map(String::into_boxed_str), key }
    }

    /// The path, when the names source gave one.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The text a refusal's field holds (`Front2Refusal`'s `function`, `adt`, ...).
    pub fn boxed(&self) -> Box<str> {
        self.to_string().into_boxed_str()
    }
}

impl fmt::Display for ItemShown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => {
                write!(f, "{p} (key ")?;
                self.key[TAIL].iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
            None => {
                write!(f, "an item the names source holds no path for (key ")?;
                self.key.iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
        }
    }
}

/// `k` as a refusal's field holds it ([`ItemShown::boxed`]), over a body's records: what
/// program.rs passes to `Front2Refusal` where it named the item by `reach::fn_shown` (its bare
/// name, which stays the name rows and symbols are made from).
pub(crate) fn refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str> {
    ItemShown::of(r.names, &*r.keys, k).boxed()
}

/// `lib`'s path: its owner's (a member's trait or impl, a variant's enum, else itself) module
/// path and name, then its own name. `None`: the source names neither the item nor its owner.
fn lib_path<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<String> {
    let (owner, last) = match names.member_owner(lib) {
        Some((owner, name)) => (owner, Some(name)),
        None => match names.variant_of(lib) {
            Some((enum_, _)) => (enum_, Some(names.name_of(lib)?)),
            None => (lib, None),
        },
    };
    let mut out = String::new();
    match names.home_of(owner) {
        Some(m) => match names.module_path(m) {
            Some(p) if !p.is_empty() => {
                out.push_str(p);
                out.push_str("::");
            }
            Some(_) => {}
            None => out.push_str("<a module the names source holds no path for>::"),
        },
        None => {}
    }
    match names.name_of(owner) {
        Some(n) => out.push_str(n),
        // An impl has no name: its header as written, whole.
        None => {
            let header = names.text_of(owner)?.impl_header?;
            out.push('<');
            out.push_str(header.trim());
            out.push('>');
        }
    }
    if let Some(l) = last {
        out.push_str("::");
        out.push_str(l);
    }
    Some(out)
}

/// What a want asks for, by name: `the head of a::B (key ..)`.
fn want_shown<'n, S: NamesSource<'n>>(names: S, w: Want) -> String {
    let (what, k) = match w {
        Want::Head(k) => ("the head of", k),
        Want::Adt(k) => ("the ADT head of", k),
        Want::Impl(k) => ("the header of", k),
        Want::Fn(k) => ("the signature of", k),
        Want::Members(k) => ("the members' predicates and signatures of", k),
        Want::MemberFn(k, f) => return format!("the signature of member fn (key {:032x}) of {}", f.0, ItemShown::of_lib(names, k)),
    };
    format!("{what} {}", ItemShown::of_lib(names, k))
}

/// [`HeadError`]'s text with each key shown by its path ([`ItemShown`]): what its `Display`
/// says, which holds no names and so prints keys. An [`HeadError::Item`] reads
/// `item a::B (key ..): <why>`, so a census that cuts at the first `"): "` keeps one class per
/// reason.
pub fn head_error_shown<'n, S: NamesSource<'n>>(names: S, e: &HeadError) -> String {
    let item = |k: LibKey| ItemShown::of_lib(names, k);
    match e {
        HeadError::NoText(k) => format!("the names library holds no text for item {}", item(*k)),
        HeadError::Cfg(k) => format!("item {} asks for cfg atoms its crate's set does not answer", item(*k)),
        HeadError::NotAHead(k, what) => format!("item {} is {what}, not a struct, enum, union, trait, impl or fn", item(*k)),
        HeadError::Cycle(ks) => {
            let mut out = String::from("these items need each other's heads in a cycle: [");
            for (i, k) in ks.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&item(*k).to_string());
            }
            out.push(']');
            out
        }
        HeadError::NeedMade { need, by } => format!(
            "{} needs {}, which is made, yet asks for it again (its lookup reads what that want does not make)",
            want_shown(names, *by),
            want_shown(names, *need)
        ),
        HeadError::Key(c) => format!("{c}"),
        HeadError::Item(k, why) => format!("item {}: {why:?}", item(*k)),
    }
}
