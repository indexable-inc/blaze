//! One item's parts of `items(file)`. An item has two, so that a reader of its signature and
//! a reader of its body are readers of different parts (a body edit reaches no reader of the
//! header, a header edit none of the body):
//!
//! - under the item's id (16 bytes), its [`Entry`]: what it is, the hash of its header, what
//!   gates it, and the items taken out of it;
//! - under [`body_key`] (the id and the byte [`BODY`], 17 bytes), its [`Body`]: the body's
//!   syntax record, its tokens (comments and whitespace left out) with the id of each item
//!   taken out of it where it stood. What reads a body (`infer`) reads this one part and not
//!   `parse(file)`; the part's digest, which the kernel keeps, is the body's identity. An
//!   item with no body has no such key (a read of it sees the absence).
//!
//! Nothing in either is a position, in bytes or in tokens, so a comment or whitespace edit
//! anywhere leaves both equal, and an edit of the header leaves the body's part equal though
//! the body's tokens moved: positions are in the line part ([`crate::lines`]).
//!
//! What the second part costs: for each item with a body one more entry of the table (the
//! kernel's directory record of 40 bytes, the 17-byte key and its place in the index), and
//! the body's tokens: 7 bytes a token beside its text, so about the body's text without its
//! trivia, plus half again for short tokens. That is what makes `infer` of one body a read of
//! one part, where it was a read of the file's tree.
//!
//! Layout of an entry, little-endian: kind (`u8`), flags (`u8`), the name's length (`u16`),
//! how many children (`u32`), the gates' length in bytes (`u32`); then the container's id and
//! the header's hash (16 bytes each); then the name, the gates, and the children's ids in the
//! order written. A body's part is its record ([`Body`]).

#![blaze::sans_io]

use alloc::{format, string::String, vec::Vec};
use core::fmt;

use crate::table::{Key, hex};

/// An item's stable id: a hash of what it is under (its container's id), its kind, what names
/// it, and its index among the items of exactly that hash. Never an ordinal of the file, so
/// inserting or removing another item changes no other item's id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub Key);

impl ItemId {
    /// The file itself: its inner attributes and its top-level items.
    pub const ROOT: ItemId = ItemId([0; 16]);
    /// The key of the list of every `impl` of the file, wherever it is nested: an `impl` in a
    /// fn body or in `const _: () = { .. }` is global for coherence. Ids, in the order written.
    pub const IMPLS: ItemId = ItemId(named(1));
    /// The key of the list of every `macro_rules!` that `#[macro_export]` may put at the crate
    /// root, wherever it is nested (directly, or through a `cfg_attr`: the entry's gates say).
    pub const EXPORTS: ItemId = ItemId(named(2));
    /// The key of the list of every `mod name;` declaration of the file: what
    /// [`crate::modules`] follows.
    pub const MODS: ItemId = ItemId(named(3));

    /// Whether this is one of the list keys, which hold ids and not an [`Entry`].
    pub fn is_list(self) -> bool {
        self == ItemId::IMPLS || self == ItemId::EXPORTS || self == ItemId::MODS
    }
}

/// What follows an item's id in the key of its body's part.
pub const BODY: u8 = 1;

/// The key of the part of `items(file)` that holds the body of the item `id`: what a reader of
/// that one body reads (`Reads::part::<Items<_>>(file, &body_key(id))`).
pub fn body_key(id: ItemId) -> [u8; 17] {
    let mut key = [BODY; 17];
    key[..16].copy_from_slice(&id.0);
    key
}

/// The item a key of `items(file)` is of, and whether the key is of its body's part: none for
/// a key of another length or another last byte.
pub fn key_item(key: &[u8]) -> Option<(ItemId, bool)> {
    match key {
        [id @ .., BODY] if id.len() == 16 => Some((ItemId(id.try_into().ok()?), true)),
        id if id.len() == 16 => Some((ItemId(id.try_into().ok()?), false)),
        _ => None,
    }
}

/// In a body's record, where an item was taken out: its id follows. No token's kind is this.
pub const CHILD: u16 = 0xfffe;

/// An item's body, as its part of `items(file)` holds it, read in place: the kind of the
/// body's node (`u16`), then each piece in the order written. A token is its kind (`u16`),
/// whether it is joined to the next (`u8`: an operator followed by one, inside a token tree),
/// its length (`u32`) and its text; an item taken out is [`CHILD`] and its id.
///
/// Positions are not here (review point 7): the `n`th token is the item's own token
/// `body_span.0 + n` of its line row (`crate::lines::ItemLines`), which places it. So an edit
/// of anything but the body's tokens (a comment in it, its header, another item) leaves the
/// record byte-equal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Body<'a> {
    /// The body's node's kind (`syntax::SyntaxKind` as `u16`): a block, a const's or
    /// static's expression, a discriminant's.
    pub kind: u16,
    pieces: &'a [u8],
    tokens: u32,
}

/// One piece of a body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Piece<'a> {
    Token { kind: u16, joint: bool, text: &'a str },
    /// An item taken out of the body, where it stood.
    Child(ItemId),
}

/// One piece from the front of `bytes`, and the rest.
fn piece_at(bytes: &[u8]) -> Result<(Piece<'_>, &[u8]), String> {
    let kind = match bytes {
        [a, b, ..] => u16::from_le_bytes([*a, *b]),
        // alloc: error path
        _ => return Err(String::from("whose piece is cut short")),
    };
    let rest = &bytes[2..];
    if kind == CHILD {
        let Some(id) = rest.get(..16) else {
            // alloc: error path
            return Err(String::from("whose item taken out is cut short"));
        };
        return Ok((Piece::Child(ItemId(id.try_into().expect("16 bytes"))), &rest[16..]));
    }
    if rest.len() < 5 || rest[0] > 1 {
        // alloc: error path
        return Err(format!("whose token of kind {kind} is cut short or has spacing {:?}", rest.first()));
    }
    let len = u32_at(rest, 1) as usize;
    let Some(text) = rest.get(5..5 + len) else {
        // alloc: error path
        return Err(format!("whose token of kind {kind} runs past the record"));
    };
    let Ok(text) = core::str::from_utf8(text) else {
        // alloc: error path
        return Err(format!("whose token of kind {kind} is not UTF-8"));
    };
    Ok((Piece::Token { kind, joint: rest[0] == 1, text }, &rest[5 + len..]))
}

impl<'a> Body<'a> {
    /// The body `bytes` are, walked once; other bytes are refused saying what is wrong.
    pub fn read(bytes: &'a [u8]) -> Result<Body<'a>, String> {
        let [a, b, pieces @ ..] = bytes else {
            // alloc: error path
            return Err(format!("an item's body of {} bytes, under the 2 of its kind", bytes.len()));
        };
        let (mut rest, mut tokens) = (pieces, 0u32);
        while !rest.is_empty() {
            let (piece, after) = piece_at(rest).map_err(|what| format!("an item's body {what}"))?;
            tokens += u32::from(matches!(piece, Piece::Token { .. }));
            rest = after;
        }
        Ok(Body { kind: u16::from_le_bytes([*a, *b]), pieces, tokens })
    }

    /// How many tokens: the body's count of own tokens (`body_span.1`).
    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    /// The pieces, in the order written.
    pub fn pieces(&self) -> impl Iterator<Item = Piece<'a>> + Clone + 'a {
        let mut rest = self.pieces;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let (piece, after) = piece_at(rest).expect("`Body::read` walked the pieces");
            rest = after;
            Some(piece)
        })
    }
}

const fn named(n: u8) -> Key {
    let mut key = [0xff; 16];
    key[15] = n;
    key
}

impl fmt::Debug for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ItemId({})", hex(&self.0))
    }
}

/// What an entry is of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The file: [`ItemId::ROOT`].
    File = 0,
    Fn = 1,
    Const = 2,
    Static = 3,
    Struct = 4,
    Union = 5,
    Enum = 6,
    /// An enum's variant that has a discriminant: its body. A variant without one has no
    /// entry; the enum's header covers every variant's tokens but the discriminants.
    Variant = 7,
    Trait = 8,
    TypeAlias = 9,
    Impl = 10,
    Mod = 11,
    Use = 12,
    ExternCrate = 13,
    ExternBlock = 14,
    MacroCall = 15,
    MacroRules = 16,
    MacroDef = 17,
    /// `global_asm!`'s item.
    Asm = 18,
}

impl Kind {
    pub fn from_u8(byte: u8) -> Option<Kind> {
        Some(match byte {
            0 => Kind::File,
            1 => Kind::Fn,
            2 => Kind::Const,
            3 => Kind::Static,
            4 => Kind::Struct,
            5 => Kind::Union,
            6 => Kind::Enum,
            7 => Kind::Variant,
            8 => Kind::Trait,
            9 => Kind::TypeAlias,
            10 => Kind::Impl,
            11 => Kind::Mod,
            12 => Kind::Use,
            13 => Kind::ExternCrate,
            14 => Kind::ExternBlock,
            15 => Kind::MacroCall,
            16 => Kind::MacroRules,
            17 => Kind::MacroDef,
            18 => Kind::Asm,
            _ => return None,
        })
    }
}

/// The item has a body of its own (a `fn`'s block, a `const`'s or `static`'s value, a
/// variant's discriminant): its hash is the part under [`body_key`].
pub const HAS_BODY: u8 = 1;
/// A `cfg` gates the item: its predicate is among the gates.
pub const CFG: u8 = 2;
/// A `cfg_attr` is on the item: what it writes depends on the crate's cfg.
pub const CFG_ATTR: u8 = 4;
/// The item is not split: `header` is the hash of its whole token stream, spacing included,
/// which is a macro's input (an item under an attribute that may be a macro, a macro call, a
/// macro's definition). It has no body hash and no children: what it holds is known only
/// after the macro has run.
pub const UNSPLIT: u8 = 8;
/// A `macro_rules!` that `#[macro_export]` may put at the crate root.
pub const EXPORTED: u8 = 16;
/// A `mod name;` declaration: its items are another file's.
pub const DECLARED: u8 = 32;
/// Every bit of the flags.
pub const FLAGS: u8 = 63;

/// The class of a gate: an attribute that gates or transforms its item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Gate {
    /// `cfg(predicate)`.
    Cfg = 1,
    /// `cfg_attr(predicate, attrs..)`.
    CfgAttr = 2,
    /// An attribute that is not built in: it may be a macro, and the item is its input.
    Macro = 3,
    /// `path = "file"`, on a `mod`.
    Path = 4,
}

/// Bytes before the ids and the name.
const HEAD: usize = 12;
const FIXED: usize = HEAD + 32;

/// An entry being written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryBuf {
    pub kind: Kind,
    pub flags: u8,
    pub container: ItemId,
    pub header: Key,
    pub name: String,
    /// Each gate: its class (`u8`), its count of tokens (`u32`), then each token as its length
    /// (`u32`) and its text.
    pub gates: Vec<u8>,
    pub children: Vec<ItemId>,
}

impl EntryBuf {
    /// The entry's bytes. A name too long for its length word is refused, naming the item.
    pub fn write(&self) -> Result<Vec<u8>, String> {
        if self.name.len() > u16::MAX as usize {
            // alloc: error path
            return Err(format!(
                "a {:?} whose name is {} bytes, past the {} an entry holds",
                self.kind,
                self.name.len(),
                u16::MAX
            ));
        }
        // alloc: one entry's bytes, kept in the step's value
        let mut out = Vec::with_capacity(
            FIXED + self.name.len() + self.gates.len() + self.children.len() * 16,
        );
        out.push(self.kind as u8);
        out.push(self.flags);
        out.extend_from_slice(&(self.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.children.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.gates.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.container.0);
        out.extend_from_slice(&self.header);
        out.extend_from_slice(self.name.as_bytes());
        out.extend_from_slice(&self.gates);
        for child in &self.children {
            out.extend_from_slice(&child.0);
        }
        Ok(out)
    }
}

/// An entry read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub kind: Kind,
    pub flags: u8,
    pub container: ItemId,
    /// Everything a reader of the signature needs: the item's tokens outside its body and
    /// outside the items taken out of it, with the id of each item taken out where it was.
    /// Of an unsplit item ([`UNSPLIT`]), its whole token stream.
    pub header: Key,
    pub name: &'a str,
    gates: &'a [u8],
    children: &'a [u8],
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn key_at(bytes: &[u8], at: usize) -> Key {
    bytes[at..at + 16].try_into().expect("16 bytes")
}

impl<'a> Entry<'a> {
    /// The entry `bytes` are; bytes [`EntryBuf::write`] did not make are refused saying what
    /// is wrong with them.
    pub fn read(bytes: &'a [u8]) -> Result<Entry<'a>, String> {
        if bytes.len() < FIXED {
            // alloc: error path
            return Err(format!("an item entry of {} bytes, under its fixed {FIXED}", bytes.len()));
        }
        let Some(kind) = Kind::from_u8(bytes[0]) else {
            // alloc: error path
            return Err(format!("an item entry of kind {}, which is none", bytes[0]));
        };
        let flags = bytes[1];
        if flags & !FLAGS != 0 {
            // alloc: error path
            return Err(format!("an item entry with flags {flags:#x}, past {FLAGS:#x}"));
        }
        let name_len = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
        let children = u32_at(bytes, 4) as usize;
        let gates_len = u32_at(bytes, 8) as usize;
        let size = FIXED as u64 + name_len as u64 + gates_len as u64 + children as u64 * 16;
        if bytes.len() as u64 != size {
            // alloc: error path
            return Err(format!(
                "an item entry of {} bytes whose parts are {size} ({name_len} of name, \
                 {gates_len} of gates, {children} children)",
                bytes.len()
            ));
        }
        let name_end = FIXED + name_len;
        let Ok(name) = core::str::from_utf8(&bytes[FIXED..name_end]) else {
            // alloc: error path
            return Err(String::from("an item entry whose name is not UTF-8"));
        };
        let entry = Entry {
            kind,
            flags,
            container: ItemId(key_at(bytes, HEAD)),
            header: key_at(bytes, HEAD + 16),
            name,
            gates: &bytes[name_end..name_end + gates_len],
            children: &bytes[name_end + gates_len..],
        };
        // Walk the gates once, so that `gates()` never meets a torn one.
        let mut rest = entry.gates;
        while !rest.is_empty() {
            rest = gate_at(rest).map_err(|what| format!("an item entry {what}"))?.2;
        }
        Ok(entry)
    }

    pub fn is(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// Whether the item has a body: a part under [`body_key`].
    pub fn has_body(&self) -> bool {
        self.is(HAS_BODY)
    }

    /// The items taken out of this one (a module's items, an `impl`'s or a `trait`'s assoc
    /// items, an `extern` block's, the items written in its body or its header), in the order
    /// written.
    pub fn children(&self) -> impl ExactSizeIterator<Item = ItemId> + Clone + 'a {
        self.children.chunks_exact(16).map(|id| ItemId(id.try_into().expect("16 bytes")))
    }

    /// The attributes that gate or transform the item, in the order written, outer then inner.
    pub fn gates(&self) -> Gates<'a> {
        Gates { rest: self.gates }
    }
}

/// One gate from the front of `bytes`: its class, its tokens' bytes, and the rest.
fn gate_at(bytes: &[u8]) -> Result<(Gate, GateTokens<'_>, &[u8]), String> {
    if bytes.len() < 5 {
        // alloc: error path
        return Err(String::from("whose gate is cut short"));
    }
    let class = match bytes[0] {
        1 => Gate::Cfg,
        2 => Gate::CfgAttr,
        3 => Gate::Macro,
        4 => Gate::Path,
        // alloc: error path
        other => return Err(format!("whose gate is of class {other}, which is none")),
    };
    let count = u32_at(bytes, 1);
    let mut at = 5;
    for _ in 0..count {
        if bytes.len() < at + 4 {
            // alloc: error path
            return Err(String::from("whose gate's token is cut short"));
        }
        let start = at + 4;
        at = start + u32_at(bytes, at) as usize;
        if bytes.len() < at {
            // alloc: error path
            return Err(String::from("whose gate's token runs past the entry"));
        }
        if core::str::from_utf8(&bytes[start..at]).is_err() {
            // alloc: error path
            return Err(String::from("whose gate's token is not UTF-8"));
        }
    }
    Ok((class, GateTokens { rest: &bytes[5..at] }, &bytes[at..]))
}

/// The gates of an entry.
#[derive(Clone, Debug)]
pub struct Gates<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Gates<'a> {
    type Item = (Gate, GateTokens<'a>);

    fn next(&mut self) -> Option<(Gate, GateTokens<'a>)> {
        if self.rest.is_empty() {
            return None;
        }
        let (class, tokens, rest) = gate_at(self.rest).expect("`Entry::read` walked the gates");
        self.rest = rest;
        Some((class, tokens))
    }
}

/// A gate's tokens: the attribute's meta (`cfg ( unix )`, `path = "a.rs"`), comments and
/// whitespace left out.
#[derive(Clone, Debug)]
pub struct GateTokens<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for GateTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }
        let len = u32_at(self.rest, 0) as usize;
        let rest: &'a [u8] = self.rest;
        let text = core::str::from_utf8(&rest[4..4 + len]).expect("a token of a `str` is UTF-8");
        self.rest = &rest[4 + len..];
        Some(text)
    }
}

impl GateTokens<'_> {
    /// The tokens joined by one space: the canonical text of the attribute, which is the key
    /// of a read of it (a cfg predicate's) and what an error prints.
    pub fn text(&self) -> String {
        // alloc: a read's key or an error's text
        let mut out = String::new();
        for (n, token) in self.clone().enumerate() {
            if n > 0 {
                out.push(' ');
            }
            out.push_str(token);
        }
        out
    }
}

/// A list key's entry ([`ItemId::IMPLS`], [`ItemId::EXPORTS`], [`ItemId::MODS`]) as its ids.
pub fn list(bytes: &[u8]) -> Result<impl ExactSizeIterator<Item = ItemId> + Clone + '_, String> {
    if bytes.len() % 16 != 0 {
        // alloc: error path
        return Err(format!("a list of item ids in {} bytes, which is no count of 16", bytes.len()));
    }
    Ok(bytes.chunks_exact(16).map(|id| ItemId(id.try_into().expect("16 bytes"))))
}
