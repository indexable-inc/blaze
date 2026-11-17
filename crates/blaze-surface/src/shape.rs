//! [`SelfShape`]: the outermost layer of an impl's self type, as 32 portable bytes, by which
//! an impl is filed and a goal finds the impls that could apply to it.
//!
//! It is what `rustc_type_ir::fast_reject::simplify_type` gives, made portable: a definition
//! is named by its [`ItemKey`], never an interned id. Two types whose shapes differ never
//! unify; a [`Shape::Blanket`] impl (`impl<T> Tr for T`) is filed apart and applies to any.
//!
//! From each `SimplifiedType` (the impl's self type simplified with
//! `TreatParams::InstantiateWithInfer`, a goal's with `AsRigid`):
//!
//! | `SimplifiedType` | [`Shape`] |
//! |---|---|
//! | `Adt(def)`, `Foreign(def)`, `Trait(def)` (a `dyn` with principal `def`) | `Def(K(def))` |
//! | `Bool`, `Char`, `Str`, `Array`, `Slice`, `Never` | the variant of the same name |
//! | `Int(t)`, `Uint(t)`, `Float(t)` | `Int`, `Uint`, `Float` with the width |
//! | `Ref(m)`, `Ptr(m)` | `Ref`, `Ptr` with the mutability |
//! | `Tuple(n)`, `Function(n)` | `Tuple(n)`, `FnPtr(n)` |
//! | `MarkerTraitObject` (`dyn Send + Sync`) | `DynMarker` |
//! | `Closure(_)` (a closure, a coroutine closure, a fn item), `Coroutine(_)`, `CoroutineWitness(_)` | `Closure`, `Coroutine`, `CoroutineWitness`: markers, no def |
//! | `UnsafeBinder`, `Placeholder` | the variant of the same name |
//! | a pattern type (`ty::Pat`, which `simplify_type` gives as `Pat`) | `Pat` |
//! | `None` (a parameter, or an alias under `InstantiateWithInfer`) | `Blanket` |
//! | `Error` | none: an impl whose self type does not resolve is not filed |
//!
//! `Adt`, `Foreign` and `Trait` are one variant because the item's kind is in its key: an
//! ADT's key and a trait's are never equal, so a second tag would say nothing the key does
//! not. The closure markers drop the def: no impl names a closure's type, so a goal on one
//! finds only blanket impls under either form, and the marker keeps body-local keys out of
//! every bucket key.
//!
//! The bytes: a `Def` is its key's 32 bytes. Every other shape has a first 8 bytes of zero,
//! which no crate's key prefix is ([`crate::crate_graph::GraphError::ReservedPrefix`] refuses one), then a
//! tag at byte 8, a width or mutability at byte 9 and an arity at bytes 12..16, the rest zero.

use core::fmt;

use blaze_id::{ContentId, ItemKey, Portable};

/// A reference's or a raw pointer's mutability.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Mutability {
    Not,
    Mut,
}

/// A signed integer's width (rustc's `IntTy`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum IntWidth {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
}

/// An unsigned integer's width (rustc's `UintTy`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum UintWidth {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
}

/// A float's width (rustc's `FloatTy`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum FloatWidth {
    F16,
    F32,
    F64,
    F128,
}

/// A [`SelfShape`] as a value to match on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Shape {
    /// An ADT, a foreign type or a trait object with a principal: the definition's key.
    Def(ItemKey),
    Bool,
    Char,
    Int(IntWidth),
    Uint(UintWidth),
    Float(FloatWidth),
    Str,
    Array,
    Slice,
    Ref(Mutability),
    Ptr(Mutability),
    Never,
    /// A tuple of this many fields.
    Tuple(u32),
    /// A function pointer taking this many arguments.
    FnPtr(u32),
    /// A trait object of auto traits alone.
    DynMarker,
    Closure,
    Coroutine,
    CoroutineWitness,
    UnsafeBinder,
    /// A goal's rigid parameter or alias: no impl is filed under it.
    Placeholder,
    /// A type parameter: an impl for any type.
    Blanket,
    /// A pattern type (`pattern_type!(u32 is 1..)`): its impls are its own (core's pat.rs),
    /// whatever its base type is.
    Pat,
}

/// Why bytes or a key are not a [`SelfShape`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeError {
    /// A definition's key whose first 8 bytes are zero, which mark a shape with no definition.
    ReservedPrefix { key: ItemKey },
    /// Bytes no shape writes: `what` names the byte.
    Bytes { what: &'static str },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::ReservedPrefix { key } => write!(
                f,
                "the item key {key:x} begins with 8 zero bytes, which mark a self shape with no definition (blaze_surface::SelfShape)"
            ),
            ShapeError::Bytes { what } => write!(f, "not a self shape: {what}"),
        }
    }
}

impl core::error::Error for ShapeError {}

const TAG: usize = 8;
const SUB: usize = 9;
const ARITY: usize = 12;

const BOOL: u8 = 1;
const CHAR: u8 = 2;
const INT: u8 = 3;
const UINT: u8 = 4;
const FLOAT: u8 = 5;
const STR: u8 = 6;
const ARRAY: u8 = 7;
const SLICE: u8 = 8;
const REF: u8 = 9;
const PTR: u8 = 10;
const NEVER: u8 = 11;
const TUPLE: u8 = 12;
const FN_PTR: u8 = 13;
const DYN_MARKER: u8 = 14;
const CLOSURE: u8 = 15;
const COROUTINE: u8 = 16;
const COROUTINE_WITNESS: u8 = 17;
const UNSAFE_BINDER: u8 = 18;
const PLACEHOLDER: u8 = 19;
const BLANKET: u8 = 20;
const PAT: u8 = 21;

/// The domain of [`SelfShape::stable_hash`].
const HASH_DOMAIN: &str = "blaze-impls self shape 1";

/// The outermost layer of a self type, as 32 bytes: see the module's table.
///
/// Its bytes are only ever those [`SelfShape::new`] writes: it is made by `new`, or read by
/// [`SelfShape::from_bytes`], which checks them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SelfShape([u8; 32]);

// SAFETY: 32 bytes: an item key's, or a tag and plain numbers; nothing interned, no address.
unsafe impl Portable for SelfShape {}

impl SelfShape {
    /// The shape of a blanket impl's self type.
    pub const BLANKET: SelfShape = SelfShape::plain(BLANKET, 0, 0);
    /// The shape of a goal's rigid parameter or alias.
    pub const PLACEHOLDER: SelfShape = SelfShape::plain(PLACEHOLDER, 0, 0);

    const fn plain(tag: u8, sub: u8, arity: u32) -> SelfShape {
        let mut bytes = [0u8; 32];
        bytes[TAG] = tag;
        bytes[SUB] = sub;
        let arity = arity.to_le_bytes();
        bytes[ARITY] = arity[0];
        bytes[ARITY + 1] = arity[1];
        bytes[ARITY + 2] = arity[2];
        bytes[ARITY + 3] = arity[3];
        SelfShape(bytes)
    }

    /// `shape` as bytes; a `Def` whose key begins with 8 zero bytes is refused.
    pub fn new(shape: Shape) -> Result<SelfShape, ShapeError> {
        Ok(match shape {
            Shape::Def(key) => {
                if key.crate_prefix() == [0; 8] {
                    return Err(ShapeError::ReservedPrefix { key });
                }
                SelfShape(key.0.0)
            }
            Shape::Bool => SelfShape::plain(BOOL, 0, 0),
            Shape::Char => SelfShape::plain(CHAR, 0, 0),
            Shape::Int(width) => SelfShape::plain(INT, width as u8, 0),
            Shape::Uint(width) => SelfShape::plain(UINT, width as u8, 0),
            Shape::Float(width) => SelfShape::plain(FLOAT, width as u8, 0),
            Shape::Str => SelfShape::plain(STR, 0, 0),
            Shape::Array => SelfShape::plain(ARRAY, 0, 0),
            Shape::Slice => SelfShape::plain(SLICE, 0, 0),
            Shape::Ref(mutability) => SelfShape::plain(REF, mutability as u8, 0),
            Shape::Ptr(mutability) => SelfShape::plain(PTR, mutability as u8, 0),
            Shape::Never => SelfShape::plain(NEVER, 0, 0),
            Shape::Tuple(arity) => SelfShape::plain(TUPLE, 0, arity),
            Shape::FnPtr(arity) => SelfShape::plain(FN_PTR, 0, arity),
            Shape::DynMarker => SelfShape::plain(DYN_MARKER, 0, 0),
            Shape::Closure => SelfShape::plain(CLOSURE, 0, 0),
            Shape::Coroutine => SelfShape::plain(COROUTINE, 0, 0),
            Shape::CoroutineWitness => SelfShape::plain(COROUTINE_WITNESS, 0, 0),
            Shape::UnsafeBinder => SelfShape::plain(UNSAFE_BINDER, 0, 0),
            Shape::Placeholder => SelfShape::PLACEHOLDER,
            Shape::Blanket => SelfShape::BLANKET,
            Shape::Pat => SelfShape::plain(PAT, 0, 0),
        })
    }

    /// The shape `bytes` are, refused when no shape writes them.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<SelfShape, ShapeError> {
        decode(&bytes).map(|_| SelfShape(bytes))
    }

    /// The 32 bytes a record holds.
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// The shape, to match on.
    pub fn shape(self) -> Shape {
        match decode(&self.0) {
            Ok(shape) => shape,
            // `new` and `from_bytes` make every `SelfShape`, and both write only what
            // `decode` reads.
            Err(error) => unreachable!("a SelfShape holds bytes `new` wrote: {error}"),
        }
    }

    /// The definition's key, for a `Def`.
    pub fn def(self) -> Option<ItemKey> {
        match self.0[..8] == [0; 8] {
            true => None,
            false => Some(ItemKey(ContentId(self.0))),
        }
    }

    /// Whether this is [`SelfShape::BLANKET`].
    pub fn is_blanket(self) -> bool {
        self == SelfShape::BLANKET
    }

    /// Whether no impl can be filed under this shape: a blanket's (filed apart) and a
    /// placeholder's (a goal's alone).
    pub fn files_no_impl(self) -> bool {
        self.is_blanket() || self == SelfShape::PLACEHOLDER
    }

    /// A hash of the bytes, the same in every process.
    pub fn stable_hash(self) -> u64 {
        let id = ContentId::of(HASH_DOMAIN, &self.0);
        let [a, b, c, d, e, f, g, h, ..] = id.0;
        u64::from_le_bytes([a, b, c, d, e, f, g, h])
    }
}

impl fmt::Debug for SelfShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.shape() {
            Shape::Def(key) => write!(f, "Def({key})"),
            shape => fmt::Debug::fmt(&shape, f),
        }
    }
}

/// What `bytes` are, or why they are no shape.
fn decode(bytes: &[u8; 32]) -> Result<Shape, ShapeError> {
    if bytes[..8] != [0; 8] {
        return Ok(Shape::Def(ItemKey(ContentId(*bytes))));
    }
    let (tag, sub) = (bytes[TAG], bytes[SUB]);
    let arity = u32::from_le_bytes([bytes[ARITY], bytes[ARITY + 1], bytes[ARITY + 2], bytes[ARITY + 3]]);
    if bytes[SUB + 1..ARITY] != [0; 2] || bytes[ARITY + 4..] != [0; 16] {
        return Err(ShapeError::Bytes { what: "a byte past the tag, width and arity is not zero" });
    }
    let no_arity = |shape: Shape| match arity {
        0 => Ok(shape),
        _ => Err(ShapeError::Bytes { what: "an arity on a shape that has none" }),
    };
    let no_sub = |shape: Shape| match sub {
        0 => no_arity(shape),
        _ => Err(ShapeError::Bytes { what: "a width or mutability on a shape that has none" }),
    };
    let bad_sub = ShapeError::Bytes { what: "a width or mutability no shape writes" };
    let mutability = |sub: u8| match sub {
        0 => Ok(Mutability::Not),
        1 => Ok(Mutability::Mut),
        _ => Err(bad_sub),
    };
    match tag {
        BOOL => no_sub(Shape::Bool),
        CHAR => no_sub(Shape::Char),
        INT => no_arity(Shape::Int(match sub {
            0 => IntWidth::Isize,
            1 => IntWidth::I8,
            2 => IntWidth::I16,
            3 => IntWidth::I32,
            4 => IntWidth::I64,
            5 => IntWidth::I128,
            _ => return Err(bad_sub),
        })),
        UINT => no_arity(Shape::Uint(match sub {
            0 => UintWidth::Usize,
            1 => UintWidth::U8,
            2 => UintWidth::U16,
            3 => UintWidth::U32,
            4 => UintWidth::U64,
            5 => UintWidth::U128,
            _ => return Err(bad_sub),
        })),
        FLOAT => no_arity(Shape::Float(match sub {
            0 => FloatWidth::F16,
            1 => FloatWidth::F32,
            2 => FloatWidth::F64,
            3 => FloatWidth::F128,
            _ => return Err(bad_sub),
        })),
        STR => no_sub(Shape::Str),
        ARRAY => no_sub(Shape::Array),
        SLICE => no_sub(Shape::Slice),
        REF => no_arity(Shape::Ref(mutability(sub)?)),
        PTR => no_arity(Shape::Ptr(mutability(sub)?)),
        NEVER => no_sub(Shape::Never),
        TUPLE | FN_PTR if sub != 0 => Err(bad_sub),
        TUPLE => Ok(Shape::Tuple(arity)),
        FN_PTR => Ok(Shape::FnPtr(arity)),
        DYN_MARKER => no_sub(Shape::DynMarker),
        CLOSURE => no_sub(Shape::Closure),
        COROUTINE => no_sub(Shape::Coroutine),
        COROUTINE_WITNESS => no_sub(Shape::CoroutineWitness),
        UNSAFE_BINDER => no_sub(Shape::UnsafeBinder),
        PLACEHOLDER => no_sub(Shape::Placeholder),
        BLANKET => no_sub(Shape::Blanket),
        PAT => no_sub(Shape::Pat),
        _ => Err(ShapeError::Bytes { what: "a tag no shape writes" }),
    }
}
