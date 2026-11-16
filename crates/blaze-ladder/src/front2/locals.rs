//! Items declared inside a body: a fn, const, static, type, impl or `use` written in a block,
//! by hand or by a macro's expansion (core's `assert_unsafe_precondition!` writes
//! `const fn precondition_check(..)` inside a block and calls it).
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement
//! block restricts its scope to the block containing the statement. The item is not given a
//! canonical path nor are any sub-items it may declare." "There is no implicit capture of the
//! containing function's generic parameters, parameters, and local variables." "Names",
//! "Scopes", item scopes: "The name of an item declared directly in a block is in scope for the
//! entire block", before its declaration too, and shadows the outer names of its namespace.
//!
//! What is here (docs/v2/plan/design/typeck.md, "Items inside bodies"; decision D62):
//! - THE TEXT. A body's lowering is dropped with its `Body`; a local item's head borrows its
//!   text for `'n` like any item's. So when a body that holds local items is lowered, its rows
//!   are KEPT in the command's frame (made by the caller of `Front2::new`): a row's text is a
//!   range of the enclosing fn's own text where it is written there, and a copy into the frame
//!   where an expansion wrote it. A body with no local item keeps nothing.
//! - IDENTITY. A local item's library key is the names library's derivation
//!   (`blaze_scope::task::key_of`) from its ENCLOSING body's key, its kind's tag, its name and
//!   its ordinal among the rows of the same kind and name of that body, with
//!   [`ItemKey::INNER`](blaze_id::ItemKey::INNER) set. Rows are in the lowering's order, a
//!   function of the body's text alone: no key depends on which body was checked first, and an
//!   edit of the body re-keys its local items and nothing else.
//! - LOOKUP. [`Locals::find`]: the block's items, then each enclosing block's, innermost
//!   first; a local item's own body continues in the blocks around its declaration (its
//!   enclosing body's), never in that body's generics or locals; nothing found: the module.

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::{Range, RowIdx, ScopeId, View};
use blaze_mem::Frame;
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_surface::{ItemDetail, Rename, UseForm};
use blaze_typeck::ItemKey;
use parser::SyntaxKind;

use super::heads::HeadError;
use super::keys::{Keys, checker_of};
use super::types::Why;

/// The kind of a local item, from its syntax node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Fn,
    Const,
    Static,
    Struct,
    Enum,
    Union,
    Trait,
    TypeAlias,
    Impl,
    Use,
    Mod,
    ExternBlock,
    ExternCrate,
    MacroRules,
    MacroDef,
    /// A node the lowering called an item that none of the above is.
    Other,
}

impl LocalKind {
    fn of(syntax: u16) -> LocalKind {
        match blaze_file_items::syntax_kind(syntax) {
            SyntaxKind::FN => LocalKind::Fn,
            SyntaxKind::CONST => LocalKind::Const,
            SyntaxKind::STATIC => LocalKind::Static,
            SyntaxKind::STRUCT => LocalKind::Struct,
            SyntaxKind::ENUM => LocalKind::Enum,
            SyntaxKind::UNION => LocalKind::Union,
            SyntaxKind::TRAIT => LocalKind::Trait,
            SyntaxKind::TYPE_ALIAS => LocalKind::TypeAlias,
            SyntaxKind::IMPL => LocalKind::Impl,
            SyntaxKind::USE => LocalKind::Use,
            SyntaxKind::MODULE => LocalKind::Mod,
            SyntaxKind::EXTERN_BLOCK => LocalKind::ExternBlock,
            SyntaxKind::EXTERN_CRATE => LocalKind::ExternCrate,
            SyntaxKind::MACRO_RULES => LocalKind::MacroRules,
            SyntaxKind::MACRO_DEF => LocalKind::MacroDef,
            _ => LocalKind::Other,
        }
    }

    /// The tag a key is derived with: the names library's (blaze-scope task/lower.rs
    /// `kind_tag`), so a local item's key is made as a module item's is.
    fn tag(self) -> &'static str {
        match self {
            LocalKind::Fn => "Function",
            LocalKind::Const => "Const",
            LocalKind::Static => "Static",
            LocalKind::Struct => "Struct",
            LocalKind::Enum => "Enum",
            LocalKind::Union => "Union",
            LocalKind::Trait => "Trait",
            LocalKind::TypeAlias => "TypeAlias",
            LocalKind::Impl => "Impl",
            LocalKind::Use => "Use",
            LocalKind::Mod => "Mod",
            LocalKind::ExternBlock => "ExternBlock",
            LocalKind::ExternCrate => "ExternCrate",
            LocalKind::MacroRules => "MacroRules",
            LocalKind::MacroDef => "MacroDef",
            LocalKind::Other => "Other",
        }
    }

    /// Its plain Rust name, for a gap or a refusal.
    pub fn form(self) -> &'static str {
        match self {
            LocalKind::Fn => "a fn in a block",
            LocalKind::Const => "a const in a block",
            LocalKind::Static => "a static in a block",
            LocalKind::Struct => "a struct in a block",
            LocalKind::Enum => "an enum in a block",
            LocalKind::Union => "a union in a block",
            LocalKind::Trait => "a trait in a block",
            LocalKind::TypeAlias => "a type alias in a block",
            LocalKind::Impl => "an impl in a block",
            LocalKind::Use => "a use declaration in a block",
            LocalKind::Mod => "a module in a block",
            LocalKind::ExternBlock => "an extern block in a block",
            LocalKind::ExternCrate => "an extern crate in a block",
            LocalKind::MacroRules => "a macro_rules! in a block",
            LocalKind::MacroDef => "a macro 2.0 in a block",
            LocalKind::Other => "an item of another kind in a block",
        }
    }

    /// Whether a name of this kind is in the value namespace (the Reference, "Names",
    /// "Namespaces": functions, constants, statics, and a tuple or unit struct's constructor).
    fn in_values(self) -> bool {
        matches!(self, LocalKind::Fn | LocalKind::Const | LocalKind::Static | LocalKind::Struct | LocalKind::Use)
    }

    /// Whether a name of this kind is in the type namespace (types, traits, modules).
    fn in_types(self) -> bool {
        matches!(self, LocalKind::Struct | LocalKind::Enum | LocalKind::Union | LocalKind::Trait | LocalKind::TypeAlias | LocalKind::Mod | LocalKind::Use)
    }

    /// The outline entry its head is made from (as the names library's for a module item);
    /// `None` for a kind whose head the store does not make here.
    pub fn detail(self) -> Option<ItemDetail> {
        Some(match self {
            LocalKind::Fn => ItemDetail::Fn { body_holds_items: true },
            LocalKind::Const => ItemDetail::Const { body_holds_items: true },
            LocalKind::Static => ItemDetail::Static { body_holds_items: true },
            _ => return None,
        })
    }
}

/// The namespace a body path's first segment is looked up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalNs {
    Value,
    Type,
}

/// One local item of a body, as kept.
#[derive(Clone, Copy, Debug)]
pub struct Row<'n> {
    /// The block it is declared in (the body's `ScopeId`).
    pub scope: u32,
    pub kind: LocalKind,
    /// Its name; for a `use`, the name it binds; empty for an `impl`.
    pub name: &'n str,
    pub key: LibKey,
    /// Where it is in the enclosing item, item-relative (an expansion's: its outermost call).
    pub range: Range,
    /// A `use`'s path to what it binds, and whether written with a leading `::`; empty else.
    pub path: &'n [&'n str],
    pub abs: bool,
}

/// A body's local items: each block's parent, and the rows.
#[derive(Clone, Copy, Debug)]
pub struct BodyLocals<'n> {
    pub parents: &'n [Option<u32>],
    pub rows: &'n [Row<'n>],
}

/// What a local item's head is made from: its text, its module (its enclosing body's) and
/// where it is declared (the body and block whose names its own body sees beyond its own).
#[derive(Clone, Copy, Debug)]
pub struct LocalHead<'n> {
    pub lib: LibKey,
    pub kind: LocalKind,
    pub text: &'n str,
    pub module: LibKey,
    pub enclosing: ItemKey,
    pub scope: u32,
    pub range: Range,
}

/// A body names a local item of a form whose record this front end does not make yet: the
/// item, its form, and where it is in its enclosing item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalGap {
    pub item: LibKey,
    pub form: &'static str,
    pub range: Range,
}

impl core::fmt::Display for LocalGap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({:x}, at {}..{} of its enclosing item) has no record yet", self.form, self.item, self.range.start, self.range.end)
    }
}

/// The body being kept: whose it is and the text it was lowered from.
#[derive(Clone, Copy)]
pub struct Enclosing<'n> {
    /// The body's checker key.
    pub owner: ItemKey,
    /// Its item (a member's: its container).
    pub item: LibKey,
    pub krate: usize,
    pub module: LibKey,
    /// The text the body was lowered from.
    pub text: &'n str,
}

/// The local items of every body lowered in this command, kept in the command's frame.
pub struct Locals<'n> {
    frame: &'n Frame,
    bodies: HashMap<ItemKey, BodyLocals<'n>, &'n Frame>,
    heads: HashMap<ItemKey, LocalHead<'n>, &'n Frame>,
}

/// The `n`th hex digit of `k`, most significant first.
fn hex_digit(k: u128, n: usize) -> u8 {
    let d = ((k >> (124 - 4 * n)) & 0xf) as u8;
    if d < 10 { b'0' + d } else { b'a' + d - 10 }
}

impl<'n> Locals<'n> {
    pub fn new(frame: &'n Frame) -> Locals<'n> {
        Locals { frame, bodies: HashMap::new_in(frame), heads: HashMap::new_in(frame) }
    }

    /// How many bodies kept their local items.
    pub fn kept(&self) -> usize {
        self.bodies.len()
    }

    /// The local item whose checker key is `key`.
    pub fn head(&self, key: ItemKey) -> Option<&LocalHead<'n>> {
        self.heads.get(&key)
    }

    /// The local items of the body `owner`.
    pub fn body(&self, owner: ItemKey) -> Option<&BodyLocals<'n>> {
        self.bodies.get(&owner)
    }

    /// The key every local item of `at`'s body is derived under: the body's own, by its
    /// item and its checker key (a member's body is not its container's).
    fn parent(names: &Names<'_>, at: Enclosing<'_>) -> LibKey {
        let mut hex = [0u8; 32];
        for (n, b) in hex.iter_mut().enumerate() {
            *b = hex_digit(at.owner.0, n);
        }
        let name = core::str::from_utf8(&hex).unwrap_or("");
        blaze_scope::task::key_of(names.krate(at.krate).key, at.item, "Body", name, 0)
    }

    /// Keeps the local items of the body `at` lowered to `view`, once per body: their rows in
    /// the frame, their keys filed in `keys`. A body with none keeps nothing. `alloc` holds
    /// the `use` trees' scratch.
    pub fn keep(&mut self, names: &'n Names<'n>, keys: &mut Keys, at: Enclosing<'n>, view: &View<'_>, alloc: Alloc<'_>) -> Result<(), HeadError> {
        if self.bodies.contains_key(&at.owner) {
            return Ok(());
        }
        let bad = |why: Why| HeadError::Item(at.item, why);
        let scopes = view.scopes.len();
        let mut count = 0usize;
        for s in 0..scopes {
            count += view.scope(ScopeId(s as u32)).map_err(|b| bad(Why::from(b)))?.items.len as usize;
        }
        if count == 0 {
            return Ok(());
        }
        let parent = Self::parent(names, at);
        let frame = self.frame;
        let parents: &'n mut [Option<u32>] = frame.alloc_slice_fill_with(scopes, |_| None);
        let mut rows: blaze_own::Vec<Row<'n>, &'n Frame> = blaze_own::Vec::new_in(frame);
        for (s, slot) in parents.iter_mut().enumerate() {
            let scope = view.scope(ScopeId(s as u32)).map_err(|b| bad(Why::from(b)))?;
            *slot = scope.parent.map(|p| p.0);
            for i in scope.items.start..scope.items.start + scope.items.len {
                let row = view.row(RowIdx(i)).map_err(|b| bad(Why::from(b)))?;
                let it = view.local_item(row).map_err(|b| bad(Why::from(b)))?;
                let kind = LocalKind::of(it.syntax);
                let range = view.site(it.range).map_err(|t| bad(Why::from(t)))?;
                // The text: the enclosing item's own where written there, else (an
                // expansion's) a copy into the frame, kept for the command.
                let text: &'n str = match view.item_relative(it.range) {
                    Some(r) => at.text.get(r.start as usize..r.end as usize).ok_or_else(|| bad(Why::from(blaze_lower::TextAt::Malformed(it.range))))?,
                    None => frame.alloc_str(view.text_at(it.range).map_err(|t| bad(Why::from(t)))?),
                };
                let name: &'n str = match it.name {
                    Some(n) => frame.alloc_str(view.name_text(n).map_err(|t| bad(Why::from(t)))?),
                    None => "",
                };
                if kind == LocalKind::Use {
                    // A `use` binds one name per leaf; each is a row of its own.
                    let tree = use_tree(text);
                    let Some(leaves) = blaze_scope::task::use_leaves(tree, alloc) else {
                        return Err(bad(Why::Unsupported("a use tree in a block that does not read", range)));
                    };
                    for leaf in leaves.iter() {
                        let bound = match leaf.form {
                            UseForm::Named { rename: Rename::Same, .. } => leaf.segments.last().copied().unwrap_or(""),
                            UseForm::Named { rename: Rename::To(n), .. } => n,
                            // `as _` binds no name; a glob is refused where a path would need it.
                            UseForm::Named { rename: Rename::Underscore, .. } => continue,
                            UseForm::Glob => return Err(bad(Why::Unsupported("a glob use in a block", range))),
                        };
                        let path: &'n [&'n str] = frame.alloc_slice_copy(&leaf.segments);
                        rows.push(Row { scope: s as u32, kind, name: bound, key: parent, range, path, abs: leaf.abs });
                    }
                    continue;
                }
                rows.push(Row { scope: s as u32, kind, name, key: parent, range, path: &[], abs: false });
                // The key: derived from the body's, by kind, name and ordinal among the rows
                // before it of the same kind and name (text order: no dependence on which body
                // was checked first).
                let last = rows.len() - 1;
                let ordinal = rows[..last].iter().filter(|r| r.kind == kind && r.name == name).count() as u32;
                let mut key = blaze_scope::task::key_of(names.krate(at.krate).key, parent, kind.tag(), name, ordinal);
                key.0.0[8] |= LibKey::INNER;
                rows[last].key = key;
                let ck = keys.item(key).map_err(HeadError::Key)?;
                self.heads.insert(ck, LocalHead { lib: key, kind, text, module: at.module, enclosing: at.owner, scope: s as u32, range });
            }
        }
        let rows: &'n [Row<'n>] = frame.alloc_slice_copy(&rows);
        self.bodies.insert(at.owner, BodyLocals { parents, rows });
        Ok(())
    }

    /// The local item `name` names in `ns`, written in block `scope` of the body `owner`: the
    /// block's items, then each enclosing block's (innermost first); where the body is itself
    /// a local item's, then the blocks around its declaration. `None`: the module's names.
    pub fn find(&self, owner: ItemKey, scope: Option<u32>, name: &str, ns: LocalNs) -> Option<Row<'n>> {
        let mut body = owner;
        let mut at = scope;
        // Each step out leaves a local item's body: at most as many steps as local items.
        for _ in 0..=self.heads.len() {
            if let Some(b) = self.bodies.get(&body) {
                let mut s = at;
                // A chain is at most as long as the body's blocks.
                for _ in 0..=b.parents.len() {
                    let Some(here) = s else { break };
                    let wanted = |r: &&Row<'n>| r.scope == here && r.name == name && if ns == LocalNs::Value { r.kind.in_values() } else { r.kind.in_types() };
                    if let Some(r) = b.rows.iter().find(wanted) {
                        return Some(*r);
                    }
                    s = b.parents.get(here as usize).copied().flatten();
                }
            }
            let h = self.heads.get(&body)?;
            body = h.enclosing;
            at = Some(h.scope);
        }
        None
    }

    /// The local item's head, if `key` is one: its kind's outline entry, text and module.
    pub fn head_of(&self, key: LibKey) -> Option<Result<(ItemDetail, &'n str, LibKey), HeadError>> {
        let h = self.heads.get(&checker_of(key))?;
        Some(match h.kind.detail() {
            Some(d) => Ok((d, h.text, h.module)),
            None => Err(HeadError::Item(key, Why::Unsupported(h.kind.form(), h.range))),
        })
    }
}

/// A `use` item's tree: its text after `use`, without the `;` (attributes and a visibility
/// before `use` dropped).
fn use_tree(text: &str) -> &str {
    let t = text.trim_end();
    let t = t.strip_suffix(';').unwrap_or(t);
    let mut from = 0;
    // The first `use` that is a whole word.
    let bytes = t.as_bytes();
    while let Some(i) = t[from..].find("use") {
        let at = from + i;
        let before = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let after = bytes.get(at + 3).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
        if before && after {
            return t[at + 3..].trim();
        }
        from = at + 3;
    }
    t.trim()
}
