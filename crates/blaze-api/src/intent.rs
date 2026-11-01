//! Edits by meaning ([`crate::Request::Edit`]) and what they changed ([`crate::Response::Edited`]):
//! the daemon applies each to the file's tree in place and commits the bytes the same edit made
//! by hand would have written (`blaze_intent`).
//!
//! Fields are declared in their keys' order (shorter first, then bytewise), as every type of
//! the API is.

use blaze_ids::ContentId;
use blaze_vfs::Op;
use serde::{Deserialize, Serialize};

/// An item by its path in one file: names joined by `::` as written in the file (`Shape::area`,
/// `mod shapes::Circle`), an impl as `<Ty>` or `<Ty as Trait>`, a keyword before a step when
/// two items share its name (`fn area`), `@n` for the n-th of the same name and kind, a tuple
/// field by its index.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Target {
    /// The file's path in the view.
    pub file: String,
    pub item: String,
}

/// One edit by meaning. The edits of a request apply in order, each to the text the ones
/// before it left; renames come before every other edit.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Edit {
    /// The `{ .. }` of a function, impl, trait or module, braces included.
    ReplaceBody { body: String, item: Target },
    /// Everything of the item from its first token after its attributes to its body or `;`.
    ReplaceSignature { item: Target, signature: String },
    /// The whole item, or one field or variant.
    ReplaceItem { item: Target, text: String },
    /// `field` (`pub x: f32`, or `f32` for a tuple struct) after the last field of a struct,
    /// union or variant.
    AddField { field: String, strukt: Target },
    RemoveField { field: String, strukt: Target },
    /// At the end of the file, or of the module, impl or trait `into` names.
    AddItem { file: String, into: Option<String>, text: String },
    /// An item, field or variant, with its line.
    RemoveItem { item: Target },
    /// `use path;` after the file's or the module's last use, or before its first item.
    AddUse { file: String, into: Option<String>, path: String },
    /// The item and every reference to it, as the front end resolves them in the crates that
    /// can name it; an out-of-line module's file moves with it.
    Rename { to: String, item: Target },
    /// Every reference that resolves to the item `from` names, retargeted to the item `to`
    /// names, each a def path from a crate's name (`std::sync::Mutex`,
    /// `parking_lot::Mutex`), in the view's crates that can name `from`: a `use` of it imports
    /// `to` instead, a path that qualifies it is rewritten, and a path through one of the
    /// view's own imports stays, as that import moves.
    Replace { to: String, from: String },
}

/// What an edit request did: the ops it commits, and each item they touched.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Edited {
    pub ops: Vec<Op>,
    /// Sorted by file, path, then how.
    pub items: Vec<EditedItem>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct EditedItem {
    pub how: How,
    /// The item's key in the view before the edits (`blaze_items::ItemKey`, a module's
    /// `blaze_items::key::module_key`), `None` for an item the edits added or one the front end
    /// gives no key.
    pub key: Option<ContentId>,
    pub file: String,
    /// The item's path as [`Target::item`] names it, in the view before the edits (after them
    /// for an added item).
    pub path: String,
}

/// How an item changed. A container whose list of members changed is `Scope`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum How {
    Body,
    Signature,
    Fields,
    Whole,
    Added,
    Removed,
    Renamed,
    Scope,
}
